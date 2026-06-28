//! Multilingual CLIP wrapper. Image side stays on the original
//! `openai/clip-vit-base-patch32` vision encoder; text side is
//! replaced with the
//! `sentence-transformers/clip-ViT-B-32-multilingual-v1` recipe —
//! distilbert-base-multilingual-cased + a learned 768 → 512 linear
//! projection that lands in the CLIP image embedding space. The
//! original CLIP text encoder is *not* loaded.
//!
//! Why two repos / three artifacts:
//!
//! * **Images.** `openai/clip-vit-base-patch32` — same vision tower
//!   as before. Image embeddings on disk from previous index runs
//!   stay valid; we don't need to reindex.
//! * **Text.** The multilingual DistilBERT under
//!   `sentence-transformers/clip-ViT-B-32-multilingual-v1/model.safetensors`
//!   produces a 768-d sequence; mean-pooling the real (non-padded)
//!   tokens and applying the projection in
//!   `2_Dense/model.safetensors` lands in the SAME 512-d space the
//!   CLIP image encoder writes into. Cosine similarity between an
//!   image vector and a query vector is therefore comparable across
//!   the encoder swap.
//!
//! First-call cost on a cold HuggingFace cache: 155 MiB (CLIP) +
//! 538 MiB (DistilBERT) + 1.5 MiB (Dense). Subsequent boots read
//! from `~/.cache/huggingface/hub/` in a few seconds.

use std::path::PathBuf;
use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{clip, distilbert};
use hf_hub::api::sync::Api;
use hf_hub::{Repo, RepoType};
use tokenizers::Tokenizer;

use crate::error::EmbedError;
use crate::text::tokenize_for_distilbert;
use crate::EMBED_DIM;

/// Image repo — unchanged. Pinned to the safetensors-conversion PR
/// so candle's safetensors loader has a clean path.
const HF_IMAGE_REPO: &str = "openai/clip-vit-base-patch32";
const HF_IMAGE_REVISION: &str = "refs/pr/15";

/// Multilingual text encoder + projection.
const HF_TEXT_REPO: &str = "sentence-transformers/clip-ViT-B-32-multilingual-v1";
const HF_TEXT_REVISION: &str = "main";

/// Image input side of CLIP. The vision tower wants 224×224 RGB,
/// normalised by per-channel mean/std baked into the model.
const IMAGE_SIZE: usize = 224;
const IMAGE_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_72];
const IMAGE_STD: [f32; 3] = [0.268_629_54, 0.261_302_8, 0.275_777_1];

/// DistilBERT hidden size (`dim` in config.json). The Dense
/// projection then maps to [`EMBED_DIM`].
const TEXT_HIDDEN: usize = 768;

/// CLIP-base image encoder + multilingual DistilBERT text encoder +
/// 768→512 projection. Cheap to clone (`Arc` inside).
pub struct Embedder {
    inner: Arc<EmbedderInner>,
}

struct EmbedderInner {
    image_model: clip::ClipModel,
    text_model: distilbert::DistilBertModel,
    text_proj: Tensor, // (512, 768), no bias
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Load (or download then load) the image + text encoders.
    /// Blocking — call from `spawn_blocking`.
    pub fn new() -> Result<Self, EmbedError> {
        let api = Api::new().map_err(|e| EmbedError::HfHub(e.to_string()))?;
        let device = Device::Cpu;

        // ---- Image side ---------------------------------------------------
        let image_repo = api.repo(Repo::with_revision(
            HF_IMAGE_REPO.to_string(),
            RepoType::Model,
            HF_IMAGE_REVISION.to_string(),
        ));
        let image_weights: PathBuf = image_repo
            .get("model.safetensors")
            .map_err(|e| EmbedError::HfHub(format!("image weights: {e}")))?;
        let image_tensors = candle_core::safetensors::load(&image_weights, &device)?;
        let image_vb = VarBuilder::from_tensors(image_tensors, DType::F32, &device);
        let image_cfg = clip::ClipConfig::vit_base_patch32();
        let image_model = clip::ClipModel::new(image_vb, &image_cfg)?;

        // ---- Text side ----------------------------------------------------
        // Tokenizer is vendored — at ~2 MiB it's cheap, and hf-hub
        // 0.3 has a known bug with alternating revisions on the same
        // Api handle that the vendoring sidesteps (see
        // feedback_workflow Rule 1).
        const TOKENIZER_BYTES: &[u8] = include_bytes!("../assets/tokenizer.json");
        let tokenizer = Tokenizer::from_bytes(TOKENIZER_BYTES)
            .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;

        let text_repo = api.repo(Repo::with_revision(
            HF_TEXT_REPO.to_string(),
            RepoType::Model,
            HF_TEXT_REVISION.to_string(),
        ));
        // DistilBERT config — vendored (~0.5 KiB) for the same
        // reason as the tokenizer: hf-hub 0.3 falls over with a
        // misleading `RelativeUrlWithoutBase` error when an `Api`
        // handle has touched two different revisions (here:
        // `refs/pr/15` for the CLIP image weights, then `main` for
        // the text encoder). See `feedback_workflow` Rule 1.
        const TEXT_CONFIG: &str = include_str!("../assets/text_config.json");
        let text_cfg: distilbert::Config = serde_json::from_str(TEXT_CONFIG)
            .map_err(|e| EmbedError::Candle(format!("DistilBERT config parse: {e}")))?;

        let text_weights: PathBuf = text_repo
            .get("model.safetensors")
            .map_err(|e| EmbedError::HfHub(format!("text weights: {e}")))?;
        let text_tensors = candle_core::safetensors::load(&text_weights, &device)?;
        let text_vb = VarBuilder::from_tensors(text_tensors, DType::F32, &device);
        let text_model = distilbert::DistilBertModel::load(text_vb, &text_cfg)?;

        // Linear projection 768 → 512. Single tensor named
        // `linear.weight` in the safetensors file shipped under
        // `2_Dense/model.safetensors`. No bias, no activation.
        let proj_weights: PathBuf = text_repo
            .get("2_Dense/model.safetensors")
            .map_err(|e| EmbedError::HfHub(format!("text projection: {e}")))?;
        let proj_tensors = candle_core::safetensors::load(&proj_weights, &device)?;
        let text_proj = proj_tensors
            .get("linear.weight")
            .cloned()
            .ok_or_else(|| {
                EmbedError::Candle(
                    "2_Dense/model.safetensors missing `linear.weight` tensor".into(),
                )
            })?;
        // Sanity-check the projection shape so a future repo rev
        // that changes dimensions surfaces here rather than as a
        // garbled cosine ranking.
        let shape = text_proj.dims();
        if shape != [EMBED_DIM, TEXT_HIDDEN] {
            return Err(EmbedError::Candle(format!(
                "text projection shape {shape:?} ≠ expected [{EMBED_DIM}, {TEXT_HIDDEN}]"
            )));
        }

        Ok(Self {
            inner: Arc::new(EmbedderInner {
                image_model,
                text_model,
                text_proj,
                tokenizer,
                device,
            }),
        })
    }

    /// Embed a single image into a 512-d L2-normalised vector. The
    /// vision tower is `openai/clip-vit-base-patch32` — unchanged
    /// across the multilingual swap, so existing index records stay
    /// valid.
    pub fn embed_image(
        &self,
        rgb: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>, EmbedError> {
        if rgb.len() != (width as usize) * (height as usize) * 3 {
            return Err(EmbedError::BadInput(format!(
                "rgb buf len {} doesn't match width × height × 3 ({})",
                rgb.len(),
                (width as usize) * (height as usize) * 3
            )));
        }
        let img = image::RgbImage::from_raw(width, height, rgb.to_vec()).ok_or_else(|| {
            EmbedError::BadInput("image::RgbImage::from_raw failed".into())
        })?;
        let resized = image::imageops::resize(
            &img,
            IMAGE_SIZE as u32,
            IMAGE_SIZE as u32,
            image::imageops::FilterType::Lanczos3,
        );

        let mut chw = vec![0f32; 3 * IMAGE_SIZE * IMAGE_SIZE];
        for (i, pixel) in resized.pixels().enumerate() {
            let r = (pixel.0[0] as f32) / 255.0;
            let g = (pixel.0[1] as f32) / 255.0;
            let b = (pixel.0[2] as f32) / 255.0;
            chw[i] = (r - IMAGE_MEAN[0]) / IMAGE_STD[0];
            chw[IMAGE_SIZE * IMAGE_SIZE + i] = (g - IMAGE_MEAN[1]) / IMAGE_STD[1];
            chw[2 * IMAGE_SIZE * IMAGE_SIZE + i] = (b - IMAGE_MEAN[2]) / IMAGE_STD[2];
        }
        let t = Tensor::from_vec(chw, (1, 3, IMAGE_SIZE, IMAGE_SIZE), &self.inner.device)?;
        let feats = self.inner.image_model.get_image_features(&t)?;
        let v = feats.to_vec2::<f32>()?;
        let mut out = v
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::Candle("empty image features".into()))?;
        if out.len() != EMBED_DIM {
            return Err(EmbedError::Candle(format!(
                "expected {EMBED_DIM}-d, got {}",
                out.len()
            )));
        }
        l2_normalise(&mut out);
        Ok(out)
    }

    /// Embed a text query (in any of the 50+ languages
    /// `distilbert-base-multilingual-cased` was trained on) into the
    /// same 512-d space as the image embeddings, L2-normalised so
    /// cosine-similarity collapses to a dot product.
    ///
    /// Pipeline: tokenize → DistilBERT forward (returns full
    /// sequence) → mean-pool over real (non-padded) tokens via the
    /// attention mask → linear projection 768 → 512 → L2-normalise.
    /// The mean-pool + projection layout matches what the upstream
    /// sentence-transformers recipe does at training time.
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let (ids, mask) = tokenize_for_distilbert(&self.inner.tokenizer, text)?;
        let n = ids.len();
        let input_ids = Tensor::from_vec(ids, (1, n), &self.inner.device)?;

        // candle's DistilBERT attention does
        //   masked_fill(scores, attn_mask, NEG_INFINITY)
        //   .broadcast_as(scores.shape())
        // where `scores` is (B, n_heads, T, T). Two implications:
        //   1. The mask values are INVERTED vs the HuggingFace
        //      Python convention: `1 = ignore this token` (it gets
        //      NEG_INFINITY-filled), `0 = real token, attend`.
        //   2. The mask has to be shaped (B, 1, 1, T) so it
        //      broadcasts across heads and query positions to the
        //      4-D scores tensor.
        // Without that, every query gets a near-identical vector
        // because attention either drops everything (always-mask)
        // or pays attention exclusively to the right-padded zero
        // slots (always-keep with shape mismatch).
        let inverted: Vec<u32> = mask.iter().map(|m| 1u32 - *m).collect();
        let pad_mask_4d = Tensor::from_vec(inverted, (1, 1, 1, n), &self.inner.device)?;

        // DistilBERT.forward returns (B, T, H). Shape is (1, n, 768).
        let seq = self
            .inner
            .text_model
            .forward(&input_ids, &pad_mask_4d)?;

        // Mean-pool the real positions. Reuse the original
        // (un-inverted) attention mask — 1 for real token, 0 for
        // pad — broadcast it to (B, T, H) and elementwise-mul into
        // `seq` before summing.
        let real_mask = Tensor::from_vec(mask, (1, n), &self.inner.device)?
            .to_dtype(DType::F32)?
            .unsqueeze(2)?
            .broadcast_as(seq.shape())?;
        let mask_f32 = real_mask;
        let masked = (seq * &mask_f32)?;
        let summed = masked.sum(1)?; // (B, H)
        let counts = mask_f32.sum(1)?.clamp(1f32, f32::INFINITY)?; // (B, 1)
        let pooled = summed.broadcast_div(&counts)?; // (B, H)

        // Linear projection 768 → 512 (no bias). `text_proj` is
        // (512, 768); `pooled` is (1, 768). We want pooled @ proj.T
        // = (1, 512).
        let proj = self.inner.text_proj.t()?; // (768, 512)
        let projected = pooled.matmul(&proj)?; // (1, 512)

        let v = projected.to_vec2::<f32>()?;
        let mut out = v
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::Candle("empty text features".into()))?;
        if out.len() != EMBED_DIM {
            return Err(EmbedError::Candle(format!(
                "expected {EMBED_DIM}-d, got {}",
                out.len()
            )));
        }
        l2_normalise(&mut out);
        Ok(out)
    }
}

impl Clone for Embedder {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn l2_normalise(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-12 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}
