//! CLIP-base wrapper. Lazy-initialised on first call to [`Embedder::new`].
//!
//! Weights and tokenizer come from `openai/clip-vit-base-patch32`
//! (HF Hub) — first run downloads ~155 MB into `~/.cache/huggingface/hub`,
//! every subsequent process boot reads from the cache.
//!
//! The actual inference path is pure CPU (no CUDA / accelerate feature).
//! On Apple silicon CLIP-base runs in ≈150 ms per image, which is fast
//! enough that batching isn't worth the API surface — embedding lives
//! on a background `spawn_blocking` and the user-visible side reads
//! from the index file.

use std::path::PathBuf;
use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::clip;
use hf_hub::api::sync::Api;
use hf_hub::{Repo, RepoType};
use tokenizers::Tokenizer;

use crate::error::EmbedError;
use crate::text::tokenize_for_clip;
use crate::EMBED_DIM;

/// Repo on HF Hub. Pinned by name — version drift would invalidate the
/// existing index file, so any swap goes through a `Stage 12.x: bump
/// CLIP repo` commit + reindex.
const HF_REPO: &str = "openai/clip-vit-base-patch32";
/// The official `openai/clip-vit-base-patch32` weights ship only as
/// `pytorch_model.bin` on `main`. PR #15 added a `model.safetensors`
/// conversion; we pin to that revision so candle's safetensors loader
/// has a clean path. Same revision the upstream candle CLIP example
/// uses.
const HF_REVISION: &str = "refs/pr/15";

/// Image input side of CLIP. The vision tower wants 224×224 RGB,
/// normalised by per-channel mean/std baked into the model.
const IMAGE_SIZE: usize = 224;
const IMAGE_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_72];
const IMAGE_STD: [f32; 3] = [0.268_629_54, 0.261_302_8, 0.275_777_1];

/// CLIP-base image + text encoder. Cheap to clone (`Arc` inside).
pub struct Embedder {
    inner: Arc<EmbedderInner>,
}

struct EmbedderInner {
    model: clip::ClipModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Load (or download then load) the CLIP-base model + tokenizer.
    /// Blocking — call from `spawn_blocking`.
    ///
    /// First invocation hits HF Hub for ~155 MB of weights. Repeat
    /// invocations after that read from `~/.cache/huggingface/hub`
    /// in a few ms.
    pub fn new() -> Result<Self, EmbedError> {
        // Weights come from HF Hub at `refs/pr/15` (the revision that
        // added the `model.safetensors` conversion of openai/clip-vit-
        // base-patch32). Tokenizer is vendored in `assets/tokenizer.json`
        // because hf-hub 0.3 errors out with a misleading
        // "RelativeUrlWithoutBase" when alternating between revisions
        // — and the tokenizer JSON is only 2.2 MiB, well below the
        // threshold where vendoring becomes silly.
        let weights_api = Api::new().map_err(|e| EmbedError::HfHub(e.to_string()))?;
        let weights_repo = weights_api.repo(Repo::with_revision(
            HF_REPO.to_string(),
            RepoType::Model,
            HF_REVISION.to_string(),
        ));
        let weights_path: PathBuf = weights_repo
            .get("model.safetensors")
            .map_err(|e| EmbedError::HfHub(format!("weights: {e}")))?;

        const TOKENIZER_BYTES: &[u8] = include_bytes!("../assets/tokenizer.json");
        let tokenizer = Tokenizer::from_bytes(TOKENIZER_BYTES)
            .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;

        let device = Device::Cpu;
        // candle_core::safetensors::load reads the whole file into
        // owned tensors — slower than mmap but safe (no unsafe block,
        // workspace forbids them). For a ~155 MiB CLIP-base this is
        // sub-second on Apple silicon and only runs on the lazy first
        // call.
        let tensors = candle_core::safetensors::load(&weights_path, &device)?;
        let vb = VarBuilder::from_tensors(tensors, DType::F32, &device);
        let cfg = clip::ClipConfig::vit_base_patch32();
        let model = clip::ClipModel::new(vb, &cfg)?;

        Ok(Self {
            inner: Arc::new(EmbedderInner {
                model,
                tokenizer,
                device,
            }),
        })
    }

    /// Embed a single image into a 512-d L2-normalised vector.
    ///
    /// `rgb` is a row-major `width × height × 3` byte buffer (0..=255
    /// per channel). The function does its own resize / normalise so
    /// the gateway can hand us whatever resolution the coarse-layer
    /// decode produced.
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
        // Resize to 224×224 via `image` crate (uses Lanczos by default,
        // which is what the original CLIP preprocessing uses).
        let img = image::RgbImage::from_raw(width, height, rgb.to_vec()).ok_or_else(|| {
            EmbedError::BadInput("image::RgbImage::from_raw failed".into())
        })?;
        let resized = image::imageops::resize(
            &img,
            IMAGE_SIZE as u32,
            IMAGE_SIZE as u32,
            image::imageops::FilterType::Lanczos3,
        );

        // Normalise to f32 with CLIP's per-channel mean/std and lay out
        // as [3, H, W] (candle expects channel-first).
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
        let feats = self.inner.model.get_image_features(&t)?;
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

    /// Embed a text query (the user's natural-language search) into
    /// the same 512-d space. L2-normalised so cosine-similarity is a
    /// plain dot product.
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let ids = tokenize_for_clip(&self.inner.tokenizer, text)?;
        let len = ids.len();
        let t = Tensor::from_vec(ids, (1, len), &self.inner.device)?;
        let feats = self.inner.model.get_text_features(&t)?;
        let v = feats.to_vec2::<f32>()?;
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
