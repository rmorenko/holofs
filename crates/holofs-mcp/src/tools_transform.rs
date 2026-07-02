//! Wavelet-domain transform tools (Stage 12.5).
//!
//! Both tools operate at the shard level, so the work happens entirely
//! in the frequency domain — no rebuild of the source. By default they
//! return the result inline as a base64-encoded blob (good for
//! "preview through Claude"). Setting `save_as` ingests the bytes back
//! into the catalog as a new object so the result is durable.

use rmcp::handler::server::wrapper::Json;
use rmcp::ErrorData;

use crate::util::{gw_err, wrap_artifact_result};
use crate::views::{AudioFilterIn, AudioFilterOut, WaveletMixIn, WaveletMixOut};
use crate::HolofsHandler;

pub(crate) async fn wavelet_mix(
    h: &HolofsHandler,
    WaveletMixIn {
        a,
        b,
        split,
        save_as,
    }: WaveletMixIn,
) -> Result<Json<WaveletMixOut>, ErrorData> {
    let mix = h.gateway.mix_objects(&a, &b, split).await.map_err(gw_err)?;
    wrap_artifact_result(
        h,
        mix.bytes,
        "image/png",
        save_as,
        |bytes, saved| WaveletMixOut {
            a: a.clone(),
            b: b.clone(),
            split,
            width: mix.width,
            height: mix.height,
            channels: mix.channels,
            bytes_downloaded: mix.bytes_downloaded,
            decode_ms: mix.decode_ms as u64,
            saved_as: saved,
            bytes_len: bytes.len() as u64,
            content_type: "image/png".into(),
            blob_base64: bytes,
        },
    )
    .await
}

pub(crate) async fn audio_filter(
    h: &HolofsHandler,
    AudioFilterIn {
        path,
        keep_layers,
        save_as,
    }: AudioFilterIn,
) -> Result<Json<AudioFilterOut>, ErrorData> {
    if keep_layers.is_empty() {
        return Err(ErrorData::invalid_params(
            "keep_layers is empty — would yield silence",
            None,
        ));
    }
    // Build the boolean mask. Highest layer index in `keep_layers`
    // sets the mask size; anything beyond that defaults to false.
    let max_layer = keep_layers.iter().copied().max().unwrap_or(0);
    let mut keep = vec![false; usize::from(max_layer) + 1];
    for l in &keep_layers {
        if let Some(slot) = keep.get_mut(usize::from(*l)) {
            *slot = true;
        }
    }
    let out = h.gateway.filter_audio(&path, &keep).await.map_err(gw_err)?;
    wrap_artifact_result(
        h,
        out.bytes,
        "audio/wav",
        save_as,
        |bytes, saved| AudioFilterOut {
            source: path.clone(),
            kept_layers: out.kept_layers.clone(),
            nlayers: out.nlayers,
            sample_rate: out.sample_rate,
            channels: out.channels,
            bytes_downloaded: out.bytes_downloaded,
            decode_ms: out.decode_ms as u64,
            saved_as: saved,
            bytes_len: bytes.len() as u64,
            content_type: "audio/wav".into(),
            blob_base64: bytes,
        },
    )
    .await
}
