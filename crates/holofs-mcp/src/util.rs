//! Local helpers shared by the MCP tool bodies.

use rmcp::handler::server::wrapper::Json;
use rmcp::{schemars, ErrorData};
use serde::Serialize;

use crate::views::CatalogRow;
use crate::HolofsHandler;

/// Maximum body returned by `read_object_text`. The MCP client embeds the
/// payload directly into the model context, so a hard cap keeps a 200 MB
/// rogue object from blowing up the conversation.
pub(crate) const MAX_READ_TEXT_BYTES: usize = 256 * 1024;

pub(crate) fn catalog_row(path: &str, m: &holofs_model::manifest::Manifest) -> CatalogRow {
    // Total systematic-shard count across (channel, layer) buckets.
    let n_shards: u32 = m
        .shard_hashes
        .iter()
        .flat_map(|chan| chan.iter())
        .map(|layer| layer.len() as u32)
        .sum();
    CatalogRow {
        path: path.to_string(),
        kind: kind_str(m.kind),
        content_type: m.content_type.clone(),
        n_shards,
        width: m.width,
        height: m.height,
        audio_sample_rate: m.audio_sample_rate,
        created_at_unix: m.created_at_unix,
    }
}

pub(crate) fn kind_str(k: holofs_model::manifest::ObjectKind) -> String {
    use holofs_model::manifest::ObjectKind;
    match k {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
    .to_string()
}

/// Strip the `holofs:///` scheme + authority off a resource URI and
/// return the catalog path. Both `holofs:///path` (canonical) and
/// `holofs://path` (degenerate) are accepted; anything else returns
/// `None`. Empty path is also rejected — the catalog root isn't a
/// readable resource.
pub(crate) fn parse_holofs_uri(uri: &str) -> Option<String> {
    let rest = uri
        .strip_prefix("holofs:///")
        .or_else(|| uri.strip_prefix("holofs://"))?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Lowercase hex encoding. Used for shard-payload bytes when the
/// caller opts into `include_payload`. Kept local instead of pulling in
/// the `hex` crate just for one call site.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Shared "either return bytes inline as base64, or ingest them to the
/// catalog under `save_as`" helper used by `wavelet_mix` /
/// `audio_filter`. When `save_as` is set:
///   * if writes are enabled and the ingest succeeds → `saved_as` is
///     populated, `blob_base64` is empty
///   * if writes are disabled → caller gets `invalid_request` so they
///     know they need a token
///
/// When `save_as` is `None` → bytes go back base64-encoded with the
/// matching `content_type`. The closure receives `(bytes_b64, saved)`
/// and builds the typed output struct.
pub(crate) async fn wrap_artifact_result<T, F>(
    h: &HolofsHandler,
    bytes: Vec<u8>,
    _content_type: &str,
    save_as: Option<String>,
    build: F,
) -> Result<Json<T>, ErrorData>
where
    T: Serialize + schemars::JsonSchema + 'static,
    F: FnOnce(String, Option<String>) -> T,
{
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    if let Some(dest) = save_as {
        h.require_writes()?;
        let res = h
            .gateway
            .ingest_bytes(&dest, &bytes)
            .await
            .map_err(gw_err)?;
        h.gateway.embed_object_in_background(dest);
        // Return an empty blob — the durable copy is the catalog entry.
        Ok(Json(build(String::new(), Some(res.name))))
    } else {
        let b64 = STANDARD.encode(&bytes);
        Ok(Json(build(b64, None)))
    }
}

pub(crate) fn gw_err(e: holofs_gateway::GatewayError) -> ErrorData {
    use holofs_gateway::GatewayError;
    match e {
        GatewayError::NotFound => ErrorData::invalid_params("object not found", None),
        GatewayError::NotADirectory => {
            ErrorData::invalid_params("path is not a directory", None)
        }
        GatewayError::BadRequest(s) => ErrorData::invalid_params(s, None),
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_holofs_uri() {
        assert_eq!(
            parse_holofs_uri("holofs:///photos/2026/img.png"),
            Some("photos/2026/img.png".into())
        );
        assert_eq!(
            parse_holofs_uri("holofs:///note.txt"),
            Some("note.txt".into())
        );
    }

    #[test]
    fn parses_degenerate_two_slash_form() {
        // Some clients emit `holofs://path` (no triple slash). Accept it
        // so a hand-typed URI still resolves.
        assert_eq!(
            parse_holofs_uri("holofs://note.txt"),
            Some("note.txt".into())
        );
    }

    #[test]
    fn rejects_other_schemes_and_empty_path() {
        assert_eq!(parse_holofs_uri("file:///etc/passwd"), None);
        assert_eq!(parse_holofs_uri("https://example.com"), None);
        assert_eq!(parse_holofs_uri("holofs:///"), None);
        assert_eq!(parse_holofs_uri(""), None);
    }
}
