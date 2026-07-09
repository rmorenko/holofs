//! coefficient-space compositions.
//!
//! Two operations that consume decoded DWT layers and re-render:
//!
//! - [`Gateway::mix_objects`] — wavelet mix of two images at a split
//!   layer. Layers `0..=split` come from image A, layers `>split`
//!   from image B. Both must share width/height/channels/k/nlayers/
//!   per-layer sym_len and position tables.
//! - [`Gateway::filter_audio`] — per-layer audio band filter. Zeros
//!   coefficients from dropped layers before the inverse Haar so a
//!   caller can emit lowpass / highpass / single-band cuts without
//!   rebuilding the file.
//!

use std::time::Instant;

use holofs_client::{get_audio_filtered, mix_images_at_split};
use holofs_model::manifest::ObjectKind;

use crate::error::GatewayError;
use crate::util::encode_png;
use crate::Gateway;

/// Result of [`Gateway::mix_objects`] — the freshly assembled hybrid
/// PNG plus accounting fields for the caller to log / display. The
/// bytes can be streamed back to the user as-is or pushed back into
/// the catalog via [`Gateway::ingest_bytes`] for a "save as" flow.
#[derive(Debug, Clone)]
pub struct MixedImage {
    /// PNG bytes of the hybrid image.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    /// Split layer used to partition layers between the two sources.
    /// Layers `0..=split` came from `name_a`, layers `>split` from
    /// `name_b`.
    pub split_layer: u8,
    /// Total number of layers either object has (validated equal).
    pub nlayers: u8,
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
}

/// Result of [`Gateway::filter_audio`] — the freshly rendered WAV
/// plus the list of layers that actually contributed. Layers not in
/// the list were zero-filled before the inverse Haar.
#[derive(Debug, Clone)]
pub struct FilteredAudio {
    /// 16-bit PCM WAV bytes.
    pub bytes: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
    pub nlayers: u8,
    /// Indices of layers whose coefficients were preserved.
    pub kept_layers: Vec<u8>,
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
}

impl Gateway {
    /// wavelet mix. Build a hybrid PNG where DWT layers
    /// `0..=split` of every channel come from `name_a` and layers
    /// `>split` from `name_b`. Both objects must be images that share
    /// width / height / channels / k / nlayers / per-layer sym_len and
    /// position tables — `mix_images_at_split` returns
    /// `ClientError::Incompatible` otherwise.
    pub async fn mix_objects(
        &self,
        name_a: &str,
        name_b: &str,
        split: u8,
    ) -> Result<MixedImage, GatewayError> {
        let snapshot = self.catalog.read().await.clone();
        let a = snapshot
            .get(name_a)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let b = snapshot
            .get(name_b)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if a.kind != ObjectKind::Image || b.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "mix: both objects must be images".into(),
            ));
        }
        if split >= a.nlayers {
            return Err(GatewayError::BadRequest(format!(
                "mix: split={split} out of range (image has {} layers, 0..={} valid)",
                a.nlayers,
                a.nlayers.saturating_sub(1)
            )));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) = mix_images_at_split(&self.gf, &a, &b, &live, split)
            .await
            .map_err(|e| match e {
                holofs_client::ClientError::Incompatible(msg) => GatewayError::BadRequest(msg),
                other => GatewayError::Decode(format!("mix: {other}")),
            })?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, a.width, a.height);
        Ok(MixedImage {
            bytes: png,
            width: a.width,
            height: a.height,
            channels: a.channels,
            split_layer: split,
            nlayers: a.nlayers,
            bytes_downloaded: bytes_dl,
            decode_ms,
        })
    }

    /// audio layer filter. Decode the object but include
    /// coefficients only from layers whose `keep[layer]` bit is set —
    /// missing layers contribute zero before the inverse Haar. Each
    /// layer roughly maps to a frequency band (L0 = bass envelope, the
    /// highest = treble), so this lets a caller emit lowpass / highpass
    /// / single-band cuts without rebuilding the file.
    pub async fn filter_audio(
        &self,
        name: &str,
        keep: &[bool],
    ) -> Result<FilteredAudio, GatewayError> {
        let manifest = self
            .catalog
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Audio {
            return Err(GatewayError::BadRequest(
                "audio filter: object is not audio".into(),
            ));
        }
        if keep.is_empty() {
            return Err(GatewayError::BadRequest(
                "audio filter: keep list is empty (would yield silence)".into(),
            ));
        }
        if !keep.iter().any(|&b| b) {
            return Err(GatewayError::BadRequest(
                "audio filter: every layer is dropped (would yield silence)".into(),
            ));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) = get_audio_filtered(&self.gf, &manifest, &live, keep)
            .await
            .map_err(|e| GatewayError::Decode(format!("audio filter: {e}")))?;
        let decode_ms = t0.elapsed().as_millis();
        let wav = holofs_codec::audio_codec::encode_wav_16bit(&channels, manifest.audio_sample_rate);
        let kept_layers: Vec<u8> = keep
            .iter()
            .enumerate()
            .filter_map(|(i, &k)| k.then_some(i as u8))
            .collect();
        Ok(FilteredAudio {
            bytes: wav,
            sample_rate: manifest.audio_sample_rate,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            kept_layers,
            bytes_downloaded: bytes_dl,
            decode_ms,
        })
    }
}
