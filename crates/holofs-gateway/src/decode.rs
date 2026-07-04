//! HTTP-facing decode of catalog objects.
//!
//! [`Gateway::decode_object`] is the shared decode path for GET
//! requests: it dispatches on [`ObjectKind`] and returns the
//! finished bytes + the metadata `holofs-web` needs to populate
//! response headers.
//!
//! For images the decoded PNG lives in a per-`(name, max_layer)`
//! cache — [`Gateway::get_or_decode`] — because the same layer of
//! the same object gets requested repeatedly by inspect grids,
//! spotlight, and preview links. Audio/text/opaque bypass the
//! cache because their formats are cheap to re-encode and we
//! haven't seen the same access-pattern hotspots.
//!

use std::sync::Arc;
use std::time::Instant;

use holofs_model::manifest::ObjectKind;

use crate::error::GatewayError;
use crate::http_gateway::CachedFile;
use crate::util::encode_png;
use crate::Gateway;

/// Decoded object payload ready to be wrapped into an HTTP response.
///
/// `bytes` is the body the client receives; `content_type` is the MIME the
/// frontend must echo. Everything else maps to `X-Holofs-*` headers or the
/// audio/text-specific headers (`Sample-Rate`, `Channels`, `Chunks-Total`,
/// `Chunks-Missing`).
#[derive(Debug, Clone)]
pub struct DecodedObject {
    /// Final encoded body (PNG, WAV, UTF-8 text, or raw opaque bytes).
    pub bytes: Vec<u8>,
    /// `Content-Type` to put on the response.
    pub content_type: String,
    /// Object kind, useful for `X-Holofs-Kind`.
    pub kind: ObjectKind,
    /// Last layer that was decoded (image/audio only). `None` for text/opaque.
    pub max_layer: Option<u8>,
    /// Bytes pulled from cluster nodes during decode.
    pub bytes_downloaded: u64,
    /// Wall-clock decode time in milliseconds.
    pub decode_ms: u128,
    /// Audio: sample rate. `None` for non-audio.
    pub sample_rate: Option<u32>,
    /// Audio: channel count (1 or 2). `None` for non-audio.
    pub channels: Option<u8>,
    /// Text: total chunk count.
    pub chunks_total: Option<usize>,
    /// Text: number of chunks replaced by hole markers.
    pub chunks_missing: Option<usize>,
    /// Opaque: original filename for `Content-Disposition: attachment`.
    pub filename_for_disposition: Option<String>,
}

impl Gateway {
    /// Try the PNG cache first; on miss run [`Self::decode_with_autorepair`]
    /// and encode to PNG. Returns `None` if the catalog entry is gone or the
    /// decode failed.
    pub(crate) async fn get_or_decode(
        &self,
        name: &str,
        max_layer: u8,
    ) -> Option<Arc<CachedFile>> {
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&(name.to_string(), max_layer)) {
                return Some(Arc::clone(c));
            }
        }
        // The catalog snapshot + live set are taken inside
        // `decode_with_autorepair`; this branch only needs the
        // post-decode width/height (which doesn't change across
        // auto-repair since `repair_node` only rewrites
        // `shard_hashes`, not dimensions).
        let (width, height) = {
            let cat = self.catalog.lock().await;
            let m = cat.get(name)?;
            (m.width, m.height)
        };
        let t0 = Instant::now();
        let (channels, bytes) = self
            .decode_with_autorepair(name, max_layer)
            .await
            .ok()?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, width, height);
        let entry = Arc::new(CachedFile {
            bytes: png,
            max_layer,
            bytes_downloaded: bytes,
            decode_ms,
        });
        self.cache
            .lock()
            .await
            .insert((name.to_string(), max_layer), Arc::clone(&entry));
        Some(entry)
    }

    /// Decode an object for HTTP transport. Returns body bytes plus the
    /// metadata the frontend needs to populate response headers.
    ///
    /// `max_layer = None` → full quality (last layer of the manifest).
    /// `max_layer = Some(0)` → preview (image LL band or audio bass).
    /// Preview is rejected with [`GatewayError::PreviewUnsupported`] for
    /// text and opaque objects.
    pub async fn decode_object(
        &self,
        name: &str,
        max_layer: Option<u8>,
    ) -> Result<DecodedObject, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let kind = manifest.kind;

        // Preview is only meaningful for image/audio.
        if matches!(kind, ObjectKind::Text | ObjectKind::Opaque) && max_layer.is_some() {
            return Err(GatewayError::PreviewUnsupported);
        }

        match kind {
            // Directories carry no payload — never reach the decode pipeline.
            ObjectKind::Directory => Err(GatewayError::IsDirectory),
            ObjectKind::Image => {
                let layer = max_layer.unwrap_or_else(|| manifest.nlayers.saturating_sub(1));
                let entry = self
                    .get_or_decode(name, layer)
                    .await
                    .ok_or_else(|| GatewayError::Decode(format!("image decode failed: {name}")))?;
                Ok(DecodedObject {
                    bytes: entry.bytes.clone(),
                    content_type: "image/png".into(),
                    kind,
                    max_layer: Some(entry.max_layer),
                    bytes_downloaded: entry.bytes_downloaded,
                    decode_ms: entry.decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Audio => {
                let layer = max_layer.unwrap_or_else(|| manifest.nlayers.saturating_sub(1));
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let (channels, bytes_dl) = holofs_client::get_audio_object_up_to_layer(
                    &self.gf, &manifest, &live, layer,
                )
                .await
                .map_err(|e| GatewayError::Decode(format!("audio decode: {e}")))?;
                let decode_ms = t0.elapsed().as_millis();
                let wav = holofs_codec::audio_codec::encode_wav_16bit(
                    &channels,
                    manifest.audio_sample_rate,
                );
                Ok(DecodedObject {
                    bytes: wav,
                    content_type: "audio/wav".into(),
                    kind,
                    max_layer: Some(layer),
                    bytes_downloaded: bytes_dl,
                    decode_ms,
                    sample_rate: Some(manifest.audio_sample_rate),
                    channels: Some(manifest.channels),
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Text => {
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let (bytes, holes) =
                    holofs_client::get_text_object_with_holes(&self.gf, &manifest, &live)
                        .await
                        .map_err(|e| GatewayError::Decode(format!("text decode: {e}")))?;
                let decode_ms = t0.elapsed().as_millis();
                Ok(DecodedObject {
                    bytes,
                    content_type: manifest.content_type.clone(),
                    kind,
                    max_layer: None,
                    bytes_downloaded: 0,
                    decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: Some(manifest.chunk_lens.len()),
                    chunks_missing: Some(holes),
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Opaque => {
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let bytes = holofs_client::get_opaque_object(&self.gf, &manifest, &live)
                    .await
                    .map_err(|e| {
                        GatewayError::Decode(format!("opaque decode (need ≥K shards): {e}"))
                    })?;
                let decode_ms = t0.elapsed().as_millis();
                Ok(DecodedObject {
                    bytes,
                    content_type: manifest.content_type.clone(),
                    kind,
                    max_layer: None,
                    bytes_downloaded: 0,
                    decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: Some(name.to_string()),
                })
            }
        }
    }
}
