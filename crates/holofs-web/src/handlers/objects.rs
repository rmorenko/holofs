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
use axum::extract::{Extension, Path, Request};
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

/// v0.7: hard cap on the streaming PUT body size, in bytes. Enforced
/// per-request by [`put_object`] while the body streams to a
/// tempfile. Distinct from `DefaultBodyLimit` (that layer caps the
/// axum-side buffered body — which we bypass here since the body
/// goes straight to disk).
///
/// Configurable via `HOLOFS_UPLOAD_MAX_SIZE=<bytes>` — default 1 GiB
/// (four times the pre-v0.7 in-RAM cap since disk is cheap).
fn upload_max_size() -> u64 {
    std::env::var("HOLOFS_UPLOAD_MAX_SIZE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1024 * 1024 * 1024)
}

/// v0.7: monotonically-increasing counter for tempfile names inside
/// the same process. Combined with `std::process::id()` this avoids
/// tmp-name collisions under concurrent PUTs to the same target.
fn upload_tmp_path(gw: &Arc<Gateway>) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    // Uploads live next to the catalog file so they land on the
    // same filesystem — a rename would need same-mount atomicity if
    // we ever start converting the tempfile into the final on-disk
    // artifact, but for now we just read + delete.
    let storage_root = gw
        .cluster()
        .node_addrs
        .first()
        .map(|_| std::env::var("HOLOFS_STORAGE_DIR").unwrap_or_else(|_| "./holofs-data".into()))
        .unwrap_or_else(|| "./holofs-data".into());
    let dir = std::path::PathBuf::from(storage_root).join("uploads");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("upload-{pid}-{n}.tmp"))
}

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

/// `PUT /<name>` — auto-detect kind and ingest. Returns the JSON
/// `IngestResult`.
///
/// **v0.7 streaming path.** Pre-v0.7 this handler took `body: Bytes`,
/// which forced axum to buffer the entire request body into memory
/// before the handler even ran — a 200 MiB upload from a slow
/// client held 200 MiB of RSS for the duration of the transfer.
///
/// Now the body streams straight to a tempfile under
/// `<storage>/uploads/upload-<pid>-<counter>.tmp`, enforcing
/// `HOLOFS_UPLOAD_MAX_SIZE` (default 1 GiB) per request. Once the
/// last byte lands the tempfile is read into a `Vec<u8>` and handed
/// to [`Gateway::ingest_bytes`] — the RLNC + DWT codec still needs
/// a `&[u8]` slice, so peak RSS at ingest is the payload size, but
/// only for the short ingest window rather than the full slow-loris
/// upload duration. The tempfile is deleted on every exit path
/// (success, error, over-limit).
pub async fn put_object(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
    req: Request<Body>,
) -> Response {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    if !is_valid_put_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be a single segment, not health-*",
        )
            .into_response();
    }

    let max = upload_max_size();
    let tmp_path = upload_tmp_path(&gw);

    // Open the tempfile with `create_new` so a stale file from a
    // crashed previous PUT doesn't get silently reused.
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            return error_to_response(GatewayError::Decode(format!(
                "streaming upload: create tmpfile {}: {e}",
                tmp_path.display()
            )));
        }
    };

    let mut stream = req.into_body().into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(b) => b,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return error_to_response(GatewayError::Decode(format!(
                    "streaming upload: read body: {e}"
                )));
            }
        };
        let n = chunk.len() as u64;
        if written.saturating_add(n) > max {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "upload exceeds cap of {max} bytes (HOLOFS_UPLOAD_MAX_SIZE)"
                ),
            )
                .into_response();
        }
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return error_to_response(GatewayError::Decode(format!(
                "streaming upload: write tmpfile: {e}"
            )));
        }
        written += n;
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return error_to_response(GatewayError::Decode(format!(
            "streaming upload: flush tmpfile: {e}"
        )));
    }
    drop(file); // release the write handle before we read back.

    // Read the tempfile back into a Vec so the existing codec path
    // (`ingest_bytes(&[u8])`) works unchanged. Full streaming ingest
    // would require an RLNC/DWT codec pass that operates on chunks
    // — out of scope for v0.7.
    let body_bytes = match tokio::fs::read(&tmp_path).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return error_to_response(GatewayError::Decode(format!(
                "streaming upload: read-back {}: {e}",
                tmp_path.display()
            )));
        }
    };
    let _ = tokio::fs::remove_file(&tmp_path).await;

    match gw.ingest_bytes(&name, &body_bytes).await {
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
