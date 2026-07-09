//! MCP `resources/*` handlers.
//!
//! Catalog entries are exposed as MCP resources at
//! `holofs:///<path>`. `list_resources` enumerates files (directories
//! are skipped — they don't have readable content of their own);
//! `read_resource` decodes the object on demand and returns either a
//! text or base64 blob depending on the kind.

use rmcp::model::{
    Annotated, ListResourcesResult, PaginatedRequestParams, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents,
};
use rmcp::ErrorData;

use crate::util::{gw_err, kind_str, parse_holofs_uri};
use crate::HolofsHandler;

pub(crate) async fn list_resources(
    h: &HolofsHandler,
    _request: Option<PaginatedRequestParams>,
) -> Result<ListResourcesResult, ErrorData> {
    use holofs_model::manifest::ObjectKind;
    let cat = h.gateway.catalog().read().await.clone();
    let resources = cat
        .entries
        .iter()
        .filter(|(_, m)| m.kind != ObjectKind::Directory)
        .map(|(name, m)| {
            let n_shards: usize = m
                .shard_hashes
                .iter()
                .flat_map(|c| c.iter())
                .map(|l| l.len())
                .sum();
            let r = RawResource::new(format!("holofs:///{name}"), name.clone())
                .with_mime_type(m.content_type.clone())
                .with_description(format!("kind={}, shards={n_shards}", kind_str(m.kind)));
            Annotated::new(r, None)
        })
        .collect();
    let mut out = ListResourcesResult::default();
    out.resources = resources;
    Ok(out)
}

pub(crate) async fn read_resource(
    h: &HolofsHandler,
    request: ReadResourceRequestParams,
) -> Result<ReadResourceResult, ErrorData> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use holofs_model::manifest::ObjectKind;

    let uri = request.uri;
    let name = parse_holofs_uri(&uri).ok_or_else(|| {
        ErrorData::invalid_params(
            format!("unrecognised resource URI: {uri:?} (expected holofs:///<path>)"),
            None,
        )
    })?;
    let dec = h.gateway.decode_object(&name, None).await.map_err(gw_err)?;
    if dec.kind == ObjectKind::Directory {
        return Err(ErrorData::invalid_params(
            "directories are not readable resources; list children via list_catalog",
            None,
        ));
    }

    // Cap reads at 1 MiB so a single resource fetch can't blow up
    // the client's context window. Text objects keep their UTF-8
    // semantics; everything else becomes a base64 blob whose
    // mime_type is taken straight from the manifest.
    const MAX_BYTES: usize = 1024 * 1024;
    let bytes = if dec.bytes.len() > MAX_BYTES {
        &dec.bytes[..MAX_BYTES]
    } else {
        &dec.bytes[..]
    };
    let contents = match dec.kind {
        ObjectKind::Text => {
            let mut c =
                ResourceContents::text(String::from_utf8_lossy(bytes).into_owned(), uri.clone());
            c = c.with_mime_type(dec.content_type.clone());
            c
        }
        _ => {
            let b64 = STANDARD.encode(bytes);
            let mut c = ResourceContents::blob(b64, uri.clone());
            c = c.with_mime_type(dec.content_type.clone());
            c
        }
    };
    Ok(ReadResourceResult::new(vec![contents]))
}
