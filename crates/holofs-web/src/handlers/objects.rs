//! Object read/write axum handlers — the CRUD verbs on `/*path`.
//!
//! - `GET /*path` — [`get_object`] full-quality decode with Range.
//! - `GET /preview/*path` — [`get_preview`] L0-only preview with Range.
//! - `GET /preview/stream/*name` — [`preview_stream`]
//!   multipart/x-mixed-replace layer-by-layer sharpen (Stage 13.1).
//! - `PUT /*path` — [`put_object`] auto-detect + ingest.
//! - `DELETE /*path` — [`delete_object`].
//! - `GET /api/shard/:c_l_idx/*name` — [`get_shard_png`] shard-payload
//!   preview PNG.
//! - `GET /pkg/holofs_bg.wasm` — [`serve_wasm_alias`] cargo-leptos
//!   filename mismatch shim.
//!
//! Moved out of `handlers.rs` in Phase R2b.3.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use holofs_gateway::{Gateway, GatewayError};
use holofs_model::manifest::ObjectKind;

use std::convert::Infallible;

use super::response::{ingest_to_response, remove_to_response, serve_with_range};
use super::util::{
    bad_request, error_to_response, is_reserved_name, is_valid_put_name, not_found,
    render_shard_as_png,
};

/// `GET /<name>` — full-quality decode. Honours the `Range:` header per
/// RFC 9110 §14.2 — see [`serve_with_range`] for the slicing logic.
pub async fn get_object(
    Path(name): Path<String>,
    headers: HeaderMap,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if is_reserved_name(&name) {
        return not_found();
    }
    match gw.decode_object(&name, None).await {
        Ok(obj) => serve_with_range(&name, obj, &headers),
        Err(e) => error_to_response(e),
    }
}

/// `GET /preview/<name>` — L0-only preview for image/audio. Range header
/// honoured against the preview-sized body.
pub async fn get_preview(
    Path(name): Path<String>,
    headers: HeaderMap,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if is_reserved_name(&name) {
        return not_found();
    }
    match gw.decode_object(&name, Some(0)).await {
        Ok(obj) => serve_with_range(&name, obj, &headers),
        Err(e) => error_to_response(e),
    }
}

/// Stage 13.1: `GET /preview/stream/<name>` — streaming hologram.
///
/// Returns a `multipart/x-mixed-replace` body where each part is the
/// PNG of the object decoded up to a growing layer ceiling
/// (L0 → L0-L1 → … → full). Browsers display each part in turn,
/// swapping the visible `<img>` content as new parts arrive — the
/// image visibly *sharpens* without a single line of JavaScript.
///
/// Cache locality: each successive `decode_object(name, Some(layer))`
/// call reuses the cluster shards the previous call already fetched
/// for the lower layers, and the on-disk PNG cache means a second
/// visitor sees instant frames.
pub async fn preview_stream(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    use http::header::HeaderValue as HHV;
    use std::str::FromStr;

    if is_reserved_name(&name) {
        return not_found();
    }

    // Read the manifest once to know how many layers we need to walk.
    // We do not snapshot the manifest itself — gateway::decode_object
    // re-reads it from the catalog on each call, which is fine.
    let nlayers = {
        let cat = gw.catalog().lock().await;
        match cat.get(&name) {
            Some(m) if m.kind == ObjectKind::Image => m.nlayers,
            Some(_) => {
                return error_to_response(GatewayError::PreviewUnsupported);
            }
            None => return not_found(),
        }
    };
    if nlayers == 0 {
        return error_to_response(GatewayError::Decode("no layers".into()));
    }

    // Boundary string must not appear inside any PNG body; the
    // RFC 2046 alphabet plus a strong random suffix is safer than
    // a fixed string, but a static boundary works in practice because
    // PNG bodies have their own framing and never contain CRLF runs
    // matching `--<token>`.
    let boundary = "hololayer-2026-06-25";
    let gw = Arc::clone(&gw);
    let name_owned = name.clone();

    let body_stream = async_stream::stream! {
        for layer in 0..nlayers {
            match gw.decode_object(&name_owned, Some(layer)).await {
                Ok(obj) => {
                    let png = obj.bytes;
                    let mut part = Vec::with_capacity(png.len() + 128);
                    // Initial CRLF only matters before the very first
                    // boundary on some clients; we include it for
                    // safety on every part.
                    part.extend_from_slice(b"\r\n--");
                    part.extend_from_slice(boundary.as_bytes());
                    part.extend_from_slice(b"\r\nContent-Type: image/png\r\nContent-Length: ");
                    part.extend_from_slice(png.len().to_string().as_bytes());
                    part.extend_from_slice(b"\r\nX-Holofs-Layer: ");
                    part.extend_from_slice(layer.to_string().as_bytes());
                    part.extend_from_slice(b"\r\n\r\n");
                    part.extend_from_slice(&png);
                    yield Ok::<_, Infallible>(Bytes::from(part));
                }
                Err(_) => {
                    // A layer failed (e.g. too few live shards). Stop
                    // the stream — the browser keeps the last good
                    // frame displayed.
                    break;
                }
            }
        }
        // Close the multipart envelope.
        let tail = format!("\r\n--{boundary}--\r\n");
        yield Ok::<_, Infallible>(Bytes::from(tail.into_bytes()));
    };

    let content_type =
        format!("multipart/x-mixed-replace; boundary={boundary}");
    let mut resp = Response::new(Body::from_stream(body_stream));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HHV::from_str(&content_type).expect("static content-type"),
    );
    // Don't let intermediaries buffer the stream.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HHV::from_static("no-store"));
    resp
}

/// `PUT /<name>` — auto-detect kind and ingest. Returns the JSON IngestResult.
pub async fn put_object(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    if !is_valid_put_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be a single segment, not health-*",
        )
            .into_response();
    }
    match gw.ingest_bytes(&name, &body).await {
        Ok(res) => {
            gw.embed_object_in_background(name.clone());
            ingest_to_response(res)
        }
        Err(e) => error_to_response(e),
    }
}

/// `DELETE /<name>` — remove from catalog + Purge.
pub async fn delete_object(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if is_reserved_name(&name) {
        return not_found();
    }
    match gw.remove_object(&name).await {
        Ok(res) => remove_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `GET /api/shard/:c_l_idx/*name` — render one shard payload as a
/// grayscale PNG for the /inspect view.
pub async fn get_shard_png(
    Path((c_l_idx, name)): Path<(String, String)>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let trimmed = c_l_idx.trim_end_matches(".png");
    let parts: Vec<&str> = trimmed.split('_').collect();
    if parts.len() != 3 {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain")],
            "bad c_l_idx",
        )
            .into_response();
    }
    let c: u8 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => return bad_request("bad channel"),
    };
    let l: u8 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => return bad_request("bad layer"),
    };
    let idx: u32 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return bad_request("bad idx"),
    };

    let payload = match gw.shard_payload(&name, c, l, idx).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(),
        Err(e) => return error_to_response(e),
    };
    let png = render_shard_as_png(&payload.payload);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CONTENT_LENGTH, png.len())
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(png.into())
        .expect("png response build")
}

/// `GET /pkg/holofs_bg.wasm` — filename shim. wasm-bindgen's JS glue
/// hardcodes `import('holofs_bg.wasm')` but cargo-leptos 0.3.6 writes
/// the binary out as plain `<output-name>.wasm`. Without this alias
/// the wasm fetch 404s and hydrate silently never runs (visible
/// symptom: lazy folders stay stuck on "loading catalog…").
pub async fn serve_wasm_alias() -> Response {
    let path = std::path::PathBuf::from("target/site/pkg/holofs.wasm");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut resp = Response::new(axum::body::Body::from(bytes));
            let h = resp.headers_mut();
            h.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/wasm"),
            );
            h.insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-cache"),
            );
            resp
        }
        Err(_) => not_found(),
    }
}
