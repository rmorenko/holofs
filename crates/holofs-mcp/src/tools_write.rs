//! Write tool bodies (Stage 12.1).
//!
//! All four require `HOLOFS_MCP_TOKEN` on the transport — see
//! [`HolofsHandler::require_writes`]. Without that the calls return an
//! explicit `invalid_request` so the client knows it's a config issue,
//! not a transient failure.

use rmcp::ErrorData;

use crate::util::gw_err;
use crate::views::{DirIn, MoveIn, PutTextIn, WriteOk};
use crate::HolofsHandler;

pub(crate) async fn put_object_text(
    h: &HolofsHandler,
    PutTextIn {
        path,
        content,
        content_type,
    }: PutTextIn,
) -> Result<WriteOk, ErrorData> {
    h.require_writes()?;
    if content.is_empty() {
        return Err(ErrorData::invalid_params("content is empty", None));
    }
    // `ingest_bytes` chooses kind / MIME from the bytes themselves; we
    // pass the body verbatim. Custom `content_type` is recorded as a
    // hint in the response since the gateway sniffs the MIME on its
    // own — we don't override it from MCP for safety (a wrong MIME
    // would break later decodes).
    let res = h
        .gateway
        .ingest_bytes(&path, content.as_bytes())
        .await
        .map_err(gw_err)?;
    h.gateway.embed_object_in_background(path.clone());
    let note = match content_type {
        Some(ct) => Some(format!(
            "ingested {} bytes, {} shards (requested content_type='{ct}' ignored — \
             gateway sniffs MIME from payload)",
            content.len(),
            res.total_shards
        )),
        None => Some(format!(
            "ingested {} bytes, {} shards",
            content.len(),
            res.total_shards
        )),
    };
    Ok(WriteOk {
        path: res.name,
        action: "wrote".into(),
        note,
    })
}

pub(crate) async fn mkdir(
    h: &HolofsHandler,
    DirIn { path }: DirIn,
) -> Result<WriteOk, ErrorData> {
    h.require_writes()?;
    let _ = h.gateway.mkdir(&path).await.map_err(gw_err)?;
    Ok(WriteOk {
        path,
        action: "created".into(),
        note: None,
    })
}

pub(crate) async fn rmdir(
    h: &HolofsHandler,
    DirIn { path }: DirIn,
) -> Result<WriteOk, ErrorData> {
    h.require_writes()?;
    let _ = h.gateway.rmdir(&path).await.map_err(gw_err)?;
    Ok(WriteOk {
        path,
        action: "removed".into(),
        note: None,
    })
}

pub(crate) async fn mv_object(
    h: &HolofsHandler,
    MoveIn { from, to }: MoveIn,
) -> Result<WriteOk, ErrorData> {
    h.require_writes()?;
    let _ = h.gateway.rename(&from, &to).await.map_err(gw_err)?;
    Ok(WriteOk {
        path: to.clone(),
        action: "renamed".into(),
        note: Some(format!("from {from}")),
    })
}
