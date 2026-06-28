//! Axum handlers for binary object routes — server-only.
//!
//! These wrap the [`Gateway`] public API and shape the result into
//! `axum::response::Response` with the `X-Holofs-*` headers documented in
//! [`docs/api.md`](../../../docs/api.md).
//!
//! Phase 4b.2 ports four legacy routes:
//! - `GET /<name>` — full decode.
//! - `GET /preview/<name>` — L0-only preview (image/audio).
//! - `PUT /<name>` — auto-detect kind, store, persist catalog.
//! - `DELETE /<name>` — remove from catalog, Purge on nodes.
//!
//! Range support and multipart form upload are deferred to later sub-phases.

#![cfg(feature = "ssr")]

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::range::{parse_range, ByteRange, RangeOutcome};

use axum::extract::Multipart;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use std::convert::Infallible;
use std::time::Duration;

use holofs_gateway::{
    ApiStats, DecodedObject, EscrowRecoverResult, EscrowShareBytes, EscrowSplitResult,
    FingerprintInfo, Gateway, GatewayError, IngestResult, KindCounts, MkdirResult,
    RemoveResult, RenameResult, RmdirResult,
};
use holofs_model::manifest::ObjectKind;

use crate::health::HealthSnapshot;

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
    use axum::body::Body;
    use http::header::HeaderValue as HHV;

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

/// `GET /api/stats` — JSON snapshot of cluster-wide counters.
pub async fn api_stats(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    json_response(StatusCode::OK, stats_to_json(&gw.api_stats().await))
}

/// `GET /metrics` — Prometheus exposition format (text-version 0.0.4).
/// Pull-based gauges sourced from [`Gateway::api_stats`] + the per-node
/// admin-kill snapshot. Production deployments scrape this every ~15s.
pub async fn metrics(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    let stats = gw.api_stats().await;
    let cluster = gw.cluster();
    let kills = gw.admin_kills_handle().lock().await.clone();

    let mut body = String::with_capacity(2048);
    body.push_str("# HELP holofs_nodes_total Total nodes in the cluster topology.\n");
    body.push_str("# TYPE holofs_nodes_total gauge\n");
    body.push_str(&format!("holofs_nodes_total {}\n", stats.nodes_total));

    body.push_str("# HELP holofs_nodes_live Nodes not currently admin-disabled.\n");
    body.push_str("# TYPE holofs_nodes_live gauge\n");
    body.push_str(&format!("holofs_nodes_live {}\n", stats.nodes_live));

    body.push_str("# HELP holofs_objects_total Objects in the catalog, broken down by kind.\n");
    body.push_str("# TYPE holofs_objects_total gauge\n");
    let by_kind = stats.objects_by_kind;
    body.push_str(&format!(
        "holofs_objects_total{{kind=\"image\"}} {}\n",
        by_kind.image
    ));
    body.push_str(&format!(
        "holofs_objects_total{{kind=\"audio\"}} {}\n",
        by_kind.audio
    ));
    body.push_str(&format!(
        "holofs_objects_total{{kind=\"text\"}} {}\n",
        by_kind.text
    ));
    body.push_str(&format!(
        "holofs_objects_total{{kind=\"opaque\"}} {}\n",
        by_kind.opaque
    ));
    body.push_str(&format!(
        "holofs_objects_total{{kind=\"directory\"}} {}\n",
        by_kind.directory
    ));

    body.push_str("# HELP holofs_shards_total Planned shards across every object × layer × channel.\n");
    body.push_str("# TYPE holofs_shards_total gauge\n");
    body.push_str(&format!("holofs_shards_total {}\n", stats.shards_total));

    body.push_str("# HELP holofs_shards_unique Distinct shard hashes recorded across the catalog.\n");
    body.push_str("# TYPE holofs_shards_unique gauge\n");
    body.push_str(&format!("holofs_shards_unique {}\n", stats.shards_unique));

    body.push_str("# HELP holofs_dedup_savings_pct (1 - unique/total) * 100.\n");
    body.push_str("# TYPE holofs_dedup_savings_pct gauge\n");
    body.push_str(&format!(
        "holofs_dedup_savings_pct {:.2}\n",
        stats.dedup_savings_pct
    ));

    body.push_str("# HELP holofs_bytes_total Approximate stored bytes across the cluster.\n");
    body.push_str("# TYPE holofs_bytes_total gauge\n");
    body.push_str(&format!("holofs_bytes_total {}\n", stats.bytes_total));

    body.push_str("# HELP holofs_auto_repairs_total GET path hit a LayerLost error and ran an inline repair_node pass.\n");
    body.push_str("# TYPE holofs_auto_repairs_total counter\n");
    body.push_str(&format!(
        "holofs_auto_repairs_total {}\n",
        stats.auto_repairs_total
    ));

    body.push_str("# HELP holofs_auto_repair_failures_total Auto-repair attempted but the post-repair decode also failed (irrecoverable).\n");
    body.push_str("# TYPE holofs_auto_repair_failures_total counter\n");
    body.push_str(&format!(
        "holofs_auto_repair_failures_total {}\n",
        stats.auto_repair_failures_total
    ));

    body.push_str("# HELP holofs_scrub_runs_total Background scrub-task tick count.\n");
    body.push_str("# TYPE holofs_scrub_runs_total counter\n");
    body.push_str(&format!("holofs_scrub_runs_total {}\n", stats.scrub_runs_total));

    body.push_str("# HELP holofs_scrub_repairs_total Objects the background scrub repaired before any user GET tripped them.\n");
    body.push_str("# TYPE holofs_scrub_repairs_total counter\n");
    body.push_str(&format!(
        "holofs_scrub_repairs_total {}\n",
        stats.scrub_repairs_total
    ));

    body.push_str("# HELP holofs_node_admin_killed Per-node admin-kill flag (1 = disabled).\n");
    body.push_str("# TYPE holofs_node_admin_killed gauge\n");
    for (idx, killed) in kills.iter().enumerate() {
        let addr = cluster
            .node_addrs
            .get(idx)
            .map(String::as_str)
            .unwrap_or("?");
        let zone = cluster.zones.get(idx).copied().unwrap_or(0);
        body.push_str(&format!(
            "holofs_node_admin_killed{{node=\"n{idx}\",addr=\"{addr}\",zone=\"{zone}\"}} {}\n",
            u8::from(*killed)
        ));
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// `GET /api/fingerprint/*path` — JSON `{name, fingerprint, kind}`.
pub async fn api_fingerprint(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    match gw.fingerprint_of(&name).await {
        Ok(info) => json_response(StatusCode::OK, fingerprint_to_json(&info)),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/upload` (multipart) — form-friendly object upload. Fields:
/// `parent` (string, may be empty for root), `name` (optional override),
/// `file` (binary). The destination path becomes `parent/<name or
/// file.name>`. Used by the catalog page's drag-zone-style form so users
/// can add files without resorting to `curl -X PUT`. Redirects back to
/// `/?p=<parent>` on success.
pub async fn upload_form(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut parent = String::new();
    let mut name_override: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;
    let mut return_to_field = String::new();
    while let Ok(Some(field)) = form.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        let upload_filename = field.file_name().map(|s| s.to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        match field_name.as_str() {
            "parent" => parent = String::from_utf8_lossy(&bytes).to_string(),
            "name" => {
                let s = String::from_utf8_lossy(&bytes).trim().to_string();
                if !s.is_empty() {
                    name_override = Some(s);
                }
            }
            "file" => {
                file_bytes = Some(bytes.to_vec());
                original_filename = upload_filename.filter(|f| !f.is_empty());
            }
            "return_to" => {
                return_to_field = String::from_utf8_lossy(&bytes).trim().to_string();
            }
            _ => {}
        }
    }
    let Some(body) = file_bytes else {
        return bad_request("no file field");
    };
    if body.is_empty() {
        return bad_request("empty file");
    }
    let leaf = name_override
        .or(original_filename)
        .unwrap_or_else(|| "uploaded.bin".to_string());
    let path = if parent.is_empty() {
        leaf
    } else {
        format!("{parent}/{leaf}")
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let target = pick_return_to(&return_to_field, &parent);
    match gw.ingest_bytes(&path, &body).await {
        Ok(_) => {
            gw.embed_object_in_background(path);
            redirect_to(&target)
        }
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/mkdir/*path` — create a `Directory` marker at `path`. Returns
/// JSON `{path, object_id}` on success, 409 on conflict, 400 on bad input.
pub async fn mkdir(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if !is_valid_put_name(&name) {
        return bad_request("reserved or empty top segment");
    }
    match gw.mkdir(&name).await {
        Ok(res) => mkdir_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/mkdir` (form-urlencoded `parent=&name=`) — form-friendly
/// variant invoked by the inline "new folder" form on the catalog page.
/// Joins `parent` + `name`, runs the same mkdir, then 303-redirects back
/// to `/?p=<parent>` so the browser reloads with the new tile visible.
pub async fn mkdir_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let parent = parse_urlencoded_field(body_str, "parent").unwrap_or_default();
    let Some(name) = parse_urlencoded_field(body_str, "name") else {
        return bad_request("missing 'name'");
    };
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let path = if parent.is_empty() {
        name
    } else {
        format!("{parent}/{name}")
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let target = pick_return_to(&return_to_field, &parent);
    match gw.mkdir(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// `DELETE /api/rmdir/*path` — remove an empty directory entry. Returns
/// `{path, object_id}`; 409 if the directory still has children.
pub async fn rmdir(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if !is_valid_put_name(&name) {
        return bad_request("reserved or empty top segment");
    }
    match gw.rmdir(&name).await {
        Ok(res) => rmdir_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/rm` (form-urlencoded `path=`) — form-friendly variant of
/// `DELETE /<name>` for the ✕ button on file rows. Redirects to
/// `return_to` (or the parent dir) on success. Mirrors `rmdir_form`
/// but resolves to `remove_object` instead of `rmdir`. Stage 11.17.
pub async fn rm_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(path) = parse_urlencoded_field(body_str, "path") else {
        return bad_request("missing 'path'");
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = pick_return_to(&return_to_field, &parent);
    match gw.remove_object(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// Stage 12.6: `GET /api/mix.png?a=&b=&split=` — stream the
/// wavelet-mix hybrid PNG. Used by the `/mix` page as the preview
/// `<img src>`; the gateway does the actual decode + IDWT + PNG
/// encode. Returns `400` with a plain-text body on parameter or
/// compatibility errors so the preview can render an inline message.
pub async fn mix_preview(
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let qs = raw_query.unwrap_or_default();
    let a = parse_urlencoded_field(&qs, "a").unwrap_or_default();
    let b = parse_urlencoded_field(&qs, "b").unwrap_or_default();
    let split: u8 = parse_urlencoded_field(&qs, "split")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if a.is_empty() {
        return bad_request("missing 'a'");
    }
    if b.is_empty() {
        return bad_request("missing 'b'");
    }
    match gw.mix_objects(&a, &b, split).await {
        Ok(out) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            out.bytes,
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// Stage 12.6: `POST /api/mix-save` (form: a, b, split, dest) —
/// builds the hybrid and ingests it into the catalog at `dest`,
/// then 303-redirects to the catalog with `?open=<parent>` so the
/// new entry is visible.
pub async fn mix_save(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(a) = parse_urlencoded_field(body_str, "a") else {
        return bad_request("missing 'a'");
    };
    let Some(b) = parse_urlencoded_field(body_str, "b") else {
        return bad_request("missing 'b'");
    };
    let split: u8 = match parse_urlencoded_field(body_str, "split")
        .and_then(|s| s.parse().ok())
    {
        Some(n) => n,
        None => return bad_request("missing or invalid 'split'"),
    };
    let Some(dest) = parse_urlencoded_field(body_str, "dest") else {
        return bad_request("missing 'dest'");
    };
    if !is_valid_put_name(&dest) {
        return bad_request("invalid 'dest' (reserved or empty top segment)");
    }
    let out = match gw.mix_objects(&a, &b, split).await {
        Ok(o) => o,
        Err(e) => return error_to_response(e),
    };
    if let Err(e) = gw.ingest_bytes(&dest, &out.bytes).await {
        return error_to_response(e);
    }
    gw.embed_object_in_background(dest.clone());
    // Land the user on the tree view with the destination's parent
    // expanded — same pattern as mkdir form.
    let parent = dest.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = if parent.is_empty() {
        format!("/?open={}", url_encode_simple(&dest))
    } else {
        format!("/?open={}", url_encode_simple(&parent))
    };
    redirect_to(&target)
}

/// Stage 13.2: `GET /api/spotlight.png?name=...&x=N&y=N&w=N&h=N` —
/// composited PNG (L0 blur outside the ROI, full resolution inside).
/// All ROI params are floats in `[0, 1]` normalised against the image
/// width / height.
pub async fn spotlight_png(
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let Some(name) = raw.as_deref().and_then(|s| parse_urlencoded_field(s, "name")) else {
        return bad_request("missing 'name'");
    };
    let parse = |k: &str, dflt: f32| -> f32 {
        raw.as_deref()
            .and_then(|s| parse_urlencoded_field(s, k))
            .and_then(|s| s.parse().ok())
            .unwrap_or(dflt)
    };
    let roi = holofs_gateway::SpotlightRoi {
        x: parse("x", 0.35),
        y: parse("y", 0.35),
        w: parse("w", 0.3),
        h: parse("h", 0.3),
    };
    // Stage 14.1: `?mode=coeff` selects the Haar-coefficient-mask
    // variant; default `spatial` (or anything else) keeps the
    // Stage 13.2 two-pass spatial composite.
    let mode = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "mode"))
        .unwrap_or_else(|| "spatial".to_string());
    let result = if mode == "coeff" {
        gw.spotlight_coeff(&name, roi).await
    } else {
        gw.spotlight(&name, roi).await
    };
    match result {
        Ok(out) => {
            let mut resp = Response::new(axum::body::Body::from(out.bytes));
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            h.insert(
                HeaderName::from_static("x-holofs-roi-px"),
                HeaderValue::from_str(&format!(
                    "{},{},{},{}",
                    out.roi_px.0, out.roi_px.1, out.roi_px.2, out.roi_px.3
                ))
                .unwrap_or(HeaderValue::from_static("0,0,0,0")),
            );
            h.insert(
                HeaderName::from_static("x-holofs-decode-ms"),
                HeaderValue::from_str(&out.decode_ms.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            h.insert(
                HeaderName::from_static("x-holofs-bytes-downloaded"),
                HeaderValue::from_str(&out.bytes_downloaded.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            resp
        }
        Err(e) => error_to_response(e),
    }
}

/// Stage 14.0: `POST /api/gc` — sweep orphan shards from every live
/// cluster node. Returns the per-node breakdown as JSON. Synchronous
/// — the typical run on a development cluster is sub-second; large
/// clusters might want this on a background task with progress
/// streaming, but that's not Stage 14.0's scope.
pub async fn gc_orphans(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    match gw.gc_orphaned_shards().await {
        Ok(rep) => {
            let mut body = String::with_capacity(320 + rep.nodes.len() * 96);
            let emb_kept = rep
                .embeddings_kept
                .map(|n| n.to_string())
                .unwrap_or_else(|| "null".into());
            let emb_dropped = rep
                .embeddings_dropped
                .map(|n| n.to_string())
                .unwrap_or_else(|| "null".into());
            body.push_str(&format!(
                "{{\"live_hashes\":{},\"manifests_scanned\":{},\"held_total\":{},\"purged_total\":{},\"embeddings_kept\":{emb_kept},\"embeddings_dropped\":{emb_dropped},\"duration_ms\":{},\"nodes\":[",
                rep.live_hashes,
                rep.manifests_scanned,
                rep.held_total,
                rep.purged_total,
                rep.duration_ms,
            ));
            for (i, n) in rep.nodes.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&format!(
                    "{{\"idx\":{},\"addr\":\"{}\",\"held\":{},\"orphaned\":{},\"ok\":{}",
                    n.node_idx,
                    n.node_addr.replace('"', "\\\""),
                    n.held,
                    n.orphaned,
                    n.ok,
                ));
                if let Some(e) = &n.error {
                    body.push_str(&format!(",\"error\":\"{}\"", json_escape(e)));
                }
                body.push('}');
            }
            body.push_str("]}");
            (
                StatusCode::OK,
                [(http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Err(e) => error_to_response(e),
    }
}

/// Stage 13.5: alias `/pkg/holofs_bg.wasm` → the on-disk
/// `target/site/pkg/holofs.wasm`. wasm-bindgen's generated JS glue
/// hardcodes the `_bg.wasm` suffix in its `import.meta.url` fetch,
/// but cargo-leptos 0.3.6 writes the binary out as plain
/// `<output-name>.wasm`. Without this alias the wasm fetch 404s and
/// hydrate silently never runs (visible symptom: lazy folders stay
/// stuck on "loading catalog…").
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

/// Stage 13.4: `POST /api/restore` — form-friendly version restore.
/// Body: `name=<path>&id=<version_id>&return_to=<url>`. On success
/// 303-redirects to `return_to` (defaults to `/versions/<name>`).
pub async fn restore_version_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: String,
) -> Response {
    let Some(name) = parse_urlencoded_field(&body, "name") else {
        return bad_request("missing 'name'");
    };
    let Some(id) = parse_urlencoded_field(&body, "id") else {
        return bad_request("missing 'id'");
    };
    let return_to = parse_urlencoded_field(&body, "return_to")
        .unwrap_or_else(|| format!("/versions/{}", url_encode_simple(&name)));
    match gw.restore_version(&name, &id).await {
        Ok(_) => {
            // Drop any cached preview / WAV for this name now that the
            // catalog points at a different manifest.
            gw.invalidate_cache(&name).await;
            redirect_to(&return_to)
        }
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/embed_all` — kick off a one-shot bulk embed of every
/// image in the catalog that isn't in `embeddings.bin` yet. Synchronous
/// — the request hangs until the walk finishes — because the typical
/// run on a few hundred images is sub-minute and the curl caller wants
/// the final `(new, skipped)` counts to print.
pub async fn embed_all(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    if !gw.embed_enabled().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "embed feature disabled — start the server with --enable-embed",
        )
            .into_response();
    }
    match gw.embed_all_pending().await {
        Ok((new_n, skip_n)) => (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "application/json")],
            format!("{{\"new\":{new_n},\"skipped\":{skip_n}}}"),
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// `GET /api/search?q=...&limit=...` — natural-language semantic search.
/// Returns JSON list of `{name, score}` sorted by cosine descending.
pub async fn semantic_search(
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let Some(q) = raw.as_deref().and_then(|s| parse_urlencoded_field(s, "q")) else {
        return (StatusCode::BAD_REQUEST, "missing 'q' query param").into_response();
    };
    if q.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "'q' is empty").into_response();
    }
    let limit: usize = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "limit"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(200);
    if !gw.embed_enabled().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "embed feature disabled — start the server with --enable-embed",
        )
            .into_response();
    }
    let band = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "band"))
        .map(|s| holofs_gateway::SearchBand::parse(&s))
        .unwrap_or(holofs_gateway::SearchBand::Any);
    match gw.semantic_search(&q, limit, band).await {
        Ok(hits) => {
            let mut body = String::from("{\"hits\":[");
            for (i, h) in hits.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                let band_str = match h.band {
                    holofs_gateway::SearchBand::Coarse => "coarse",
                    holofs_gateway::SearchBand::Mid => "mid",
                    holofs_gateway::SearchBand::Full => "full",
                    holofs_gateway::SearchBand::Any => "any",
                };
                body.push_str(&format!(
                    "{{\"name\":\"{}\",\"score\":{:.6},\"band\":\"{band_str}\"}}",
                    h.name.replace('\\', "\\\\").replace('"', "\\\""),
                    h.score
                ));
            }
            body.push_str("]}");
            (
                StatusCode::OK,
                [(http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Err(e) => error_to_response(e),
    }
}

/// Tiny URL-encode for redirect targets — only escapes the few
/// characters that mangle a `/?open=` query value. Kept local to
/// avoid pulling another crate.
fn url_encode_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `POST /api/rmdir` (form-urlencoded `path=`) — form-friendly variant for
/// the delete button on directory cards. Redirects back to the parent
/// directory on success.
pub async fn rmdir_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(path) = parse_urlencoded_field(body_str, "path") else {
        return bad_request("missing 'path'");
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = pick_return_to(&return_to_field, &parent);
    match gw.rmdir(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// Resolve where to send the user after a form mutation. Honours an
/// explicit `return_to` field when present (the tree-view forms set
/// this to `/` so the page doesn't switch to focus mode); otherwise
/// falls back to `/?p=<parent>` for backward-compat with the
/// focus-view forms that omit the field.
fn pick_return_to(return_to: &str, parent: &str) -> String {
    if !return_to.is_empty() {
        return return_to.to_string();
    }
    if parent.is_empty() {
        "/".to_string()
    } else {
        format!("/?p={parent}")
    }
}

/// 303 redirect to an absolute or relative URL. The form-friendly
/// mkdir/rmdir/upload handlers pick the target from a hidden
/// `return_to` field so each calling page can decide where to go after
/// success — the tree-view stays on `/`, the focus-view comes back to
/// `/?p=<parent>`, etc. Falls back to `/` if no target was given.
fn redirect_to(target: &str) -> Response {
    let target = if target.is_empty() { "/" } else { target };
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, target)
        .body(axum::body::Body::empty())
        .expect("redirect build")
}

/// `POST /api/mv` — rename / move an entry. Body is form-urlencoded
/// `from=...&to=...` so the dropzone HTML form can submit it without JS.
/// Directories carry every descendant along.
pub async fn mv(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(from) = parse_urlencoded_field(body_str, "from") else {
        return bad_request("missing 'from'");
    };
    let Some(to) = parse_urlencoded_field(body_str, "to") else {
        return bad_request("missing 'to'");
    };
    if !is_valid_put_name(&from) || !is_valid_put_name(&to) {
        return bad_request("reserved or empty top segment");
    }
    match gw.rename(&from, &to).await {
        Ok(res) => rename_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `GET /api/shard/:c_l_idx/*path` — grayscale PNG of one shard's payload.
/// `c_l_idx` is the `<c>_<l>_<idx>.png` triple (`.png` optional); the
/// trailing wildcard carries the full object path. This is the Stage 9
/// successor of `/inspect/:name/shard/:c_l_idx`.
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

fn bad_request(msg: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain")],
        msg,
    )
        .into_response()
}

/// Turn arbitrary bytes into a square grayscale PNG: side = ceil(sqrt(len)),
/// tail padded with zeros. Mirrors the legacy gateway's `render_shard_as_png`.
fn render_shard_as_png(bytes: &[u8]) -> Vec<u8> {
    let side = (bytes.len() as f64).sqrt().ceil() as usize;
    let side = side.max(1);
    let total = side * side;
    let mut padded = bytes.to_vec();
    padded.resize(total, 0);
    let mut rgb = Vec::with_capacity(total * 3);
    for &g in &padded {
        rgb.extend_from_slice(&[g, g, g]);
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, side as u32, side as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&rgb).expect("png write");
    }
    buf
}

/// `POST /admin/node` — toggle admin-kill for node `i` (form field). Used by
/// the kill/revive buttons on `/health`; redirects back to `/health` (303).
pub async fn toggle_node(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "text/plain")],
                "non-utf8 body",
            )
                .into_response();
        }
    };
    let idx = parse_urlencoded_field(body_str, "i").and_then(|s| s.parse::<usize>().ok());
    let Some(idx) = idx else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain")],
            "missing or bad field i",
        )
            .into_response();
    };
    match gw.toggle_admin_kill(idx).await {
        Ok(_) => Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, "/health")
            .body(axum::body::Body::empty())
            .expect("redirect build"),
        Err(e) => error_to_response(e),
    }
}

/// Minimal `application/x-www-form-urlencoded` field extractor. Good enough
/// for the single-field admin-node form; full multipart handling stays in
/// the legacy gateway until Phase 4b.5.
fn parse_urlencoded_field(body: &str, field: &str) -> Option<String> {
    for pair in body.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next()?;
        let val = it.next().unwrap_or("");
        if key == field {
            return Some(url_decode_simple(val));
        }
    }
    None
}

/// Decode `+` → space and `%XX` → byte for a single form field.
///
/// Stage 11.13: bytes flow through a `Vec<u8>` rather than being pushed
/// straight into a `String`. The old code did `out.push(byte as char)`,
/// which treated each decoded byte as a Unicode code point — fine for
/// ASCII, garbage for any multi-byte UTF-8 sequence. A Russian "С"
/// (UTF-8 `D0 A1`) used to render as `Ð¡`; with the byte-buffer path we
/// reassemble the original UTF-8 bytes and `String::from_utf8_lossy`
/// hands back the right characters.
fn url_decode_simple(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                } else {
                    out.push(b'%');
                }
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// === Response builders ====================================================

/// Dispatch based on the request's `Range` header:
/// - none / malformed → full 200 body via [`decoded_to_response`];
/// - single satisfiable range → 206 via [`partial_response`];
/// - unsatisfiable → 416 with `Content-Range: bytes */<total>`;
/// - multi-range → degrade gracefully to a full 200.
///
/// The full decoded buffer is always materialised first — partial reads
/// are slice operations, not progressive decode. This matches the rest of
/// the gateway's read path and is enough for the workloads Range actually
/// helps with (resume, `<audio>` scrubbing).
fn serve_with_range(name: &str, obj: DecodedObject, headers: &HeaderMap) -> Response {
    let total = obj.bytes.len() as u64;
    let outcome = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| parse_range(s, total))
        .unwrap_or(RangeOutcome::NoRange);
    match outcome {
        RangeOutcome::NoRange | RangeOutcome::Multiple | RangeOutcome::Malformed => {
            decoded_to_response(name, obj)
        }
        RangeOutcome::Range(r) => partial_response(name, obj, r),
        RangeOutcome::Unsatisfiable => unsatisfiable_response(total),
    }
}

/// 416 Range Not Satisfiable. Per RFC 9110 §15.5.17 the response MUST
/// carry a `Content-Range: bytes */<total>` so the client knows the
/// resource's true size and can retry sensibly.
fn unsatisfiable_response(total: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body("range not satisfiable".into())
        .expect("416 build")
}

/// 206 Partial Content. Copies the requested slice out of `obj.bytes`,
/// emits the standard `Content-Range: bytes A-B/total` header, and
/// preserves the kind / cache / disposition headers from a full response.
fn partial_response(name: &str, obj: DecodedObject, range: ByteRange) -> Response {
    let total = obj.bytes.len() as u64;
    let start = range.start as usize;
    let end_inclusive = range.end_inclusive as usize;
    let slice = obj.bytes[start..=end_inclusive].to_vec();
    let slice_len = slice.len();

    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, &obj.content_type)
        .header(header::CONTENT_LENGTH, slice_len)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{total}", range.start, range.end_inclusive),
        )
        .header(x("x-holofs-kind"), kind_label(obj.kind))
        .header(x("x-holofs-bytes-downloaded"), obj.bytes_downloaded)
        .header(x("x-holofs-decode-ms"), obj.decode_ms as u64);

    if let Some(layer) = obj.max_layer {
        builder = builder.header(x("x-holofs-layers"), format!("0-{layer}"));
    }
    if let Some(sr) = obj.sample_rate {
        builder = builder.header(x("x-holofs-sample-rate"), sr);
    }
    if let Some(ch) = obj.channels {
        builder = builder.header(x("x-holofs-channels"), u16::from(ch));
    }
    if obj.kind == ObjectKind::Image {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=3600");
    }
    if let Some(filename) = obj.filename_for_disposition.as_deref() {
        let safe = filename.replace('"', "");
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\""),
        );
    }
    let _ = name;
    builder.body(slice.into()).expect("206 build")
}

fn decoded_to_response(name: &str, obj: DecodedObject) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &obj.content_type)
        .header(header::CONTENT_LENGTH, obj.bytes.len())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(x("x-holofs-kind"), kind_label(obj.kind))
        .header(x("x-holofs-bytes-downloaded"), obj.bytes_downloaded)
        .header(x("x-holofs-decode-ms"), obj.decode_ms as u64);

    if let Some(layer) = obj.max_layer {
        builder = builder.header(x("x-holofs-layers"), format!("0-{layer}"));
    }
    if let Some(sr) = obj.sample_rate {
        builder = builder.header(x("x-holofs-sample-rate"), sr);
    }
    if let Some(ch) = obj.channels {
        builder = builder.header(x("x-holofs-channels"), u16::from(ch));
    }
    if let Some(total) = obj.chunks_total {
        builder = builder.header(x("x-holofs-chunks-total"), total as u64);
    }
    if let Some(missing) = obj.chunks_missing {
        builder = builder.header(x("x-holofs-chunks-missing"), missing as u64);
    }
    if obj.kind == ObjectKind::Image {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=3600");
    }
    if let Some(filename) = obj.filename_for_disposition.as_deref() {
        let safe = filename.replace('"', "");
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\""),
        );
    }
    let _ = name;
    builder.body(obj.bytes.into()).expect("response build")
}

fn ingest_to_response(res: IngestResult) -> Response {
    let body = format!(
        "{{\"name\":\"{name}\",\
\"object_id\":\"{oid:016x}\",\
\"data_cid\":\"{cid}\",\
\"width\":{w},\"height\":{h},\
\"shards\":{shards},\
\"put_ms\":{ms}}}\n",
        name = json_escape(&res.name),
        oid = res.object_id,
        cid = res.data_cid_hex,
        w = res.width,
        h = res.height,
        shards = res.total_shards,
        ms = res.put_ms,
    );
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn remove_to_response(res: RemoveResult) -> Response {
    let body = format!(
        "{{\"deleted\":\"{name}\",\"object_id\":\"{oid:016x}\"}}\n",
        name = json_escape(&res.name),
        oid = res.object_id,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn mkdir_to_response(res: MkdirResult) -> Response {
    let body = format!(
        "{{\"created\":\"{p}\",\"object_id\":\"{oid:016x}\"}}\n",
        p = json_escape(&res.path),
        oid = res.object_id,
    );
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn rmdir_to_response(res: RmdirResult) -> Response {
    let body = format!(
        "{{\"removed\":\"{p}\",\"object_id\":\"{oid:016x}\"}}\n",
        p = json_escape(&res.path),
        oid = res.object_id,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn rename_to_response(res: RenameResult) -> Response {
    let body = format!(
        "{{\"from\":\"{f}\",\"to\":\"{t}\",\"moved\":{n}}}\n",
        f = json_escape(&res.old),
        t = json_escape(&res.new),
        n = res.moved_entries,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn error_to_response(e: GatewayError) -> Response {
    let (status, msg) = match &e {
        GatewayError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        GatewayError::BadRequest(s) => (StatusCode::BAD_REQUEST, s.clone()),
        GatewayError::Decode(s) => (StatusCode::SERVICE_UNAVAILABLE, s.clone()),
        GatewayError::PreviewUnsupported => (
            StatusCode::NOT_FOUND,
            "preview not supported for this kind".to_string(),
        ),
        GatewayError::IsDirectory => (StatusCode::CONFLICT, "is a directory".to_string()),
        GatewayError::AlreadyExists => {
            (StatusCode::CONFLICT, "already exists".to_string())
        }
        GatewayError::NotADirectory => {
            (StatusCode::CONFLICT, "not a directory".to_string())
        }
        GatewayError::DirectoryNotEmpty => {
            (StatusCode::CONFLICT, "directory not empty".to_string())
        }
    };
    (status, [(header::CONTENT_TYPE, "text/plain")], msg).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        "not found",
    )
        .into_response()
}

// === Helpers ===============================================================

fn x(s: &'static str) -> HeaderName {
    HeaderName::from_static(s)
}

fn kind_label(kind: ObjectKind) -> HeaderValue {
    HeaderValue::from_static(match kind {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    })
}

/// Top-level path segments owned by the HTTP frontend itself. An object
/// (or any of its parent directories) named with one of these would shadow
/// a real route, so PUT/DELETE/mkdir refuse them.
const RESERVED_TOP_SEGMENTS: &[&str] = &[
    "health", "escrow", "preview", "inspect", "similar", "diff", "admin", "api",
    "metrics", "pkg",
    // Stage 10: in-app docs viewer + zoom variant of /inspect.
    "help", "inspect-zoom",
    // Stage 11.5: static-asset prefix served by ServeDir.
    "assets",
    // Stage 12.6: wavelet-mix composer page.
    "mix",
    // Stage 12.7: pitch / marketing page.
    "about",
    // Stage 12.9: semantic search page.
    "search",
    // Stage 13.1: streaming hologram demo page.
    "holo",
    // Stage 13.2: ROI spotlight composite page.
    "spotlight",
    // Stage 13.4: per-object version history page.
    "versions",
];

fn top_segment(path: &str) -> &str {
    path.split('/').next().unwrap_or("")
}

/// `true` if `name`'s top-level segment collides with an HTTP route. Empty
/// name is also reserved (handled separately by path validation).
fn is_reserved_name(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    RESERVED_TOP_SEGMENTS.contains(&top_segment(name))
}

/// Names accepted by `PUT /<path>` and the directory-op endpoints. Slash is
/// allowed (paths are multi-segment after Stage 9); only the top-level
/// segment is checked against the reserved list. Structural validation
/// (dot/double-slash/length) is the gateway's job — `Gateway::ingest_bytes`
/// runs `holofs_model::path::validate` and returns `BadRequest` on failure.
fn is_valid_put_name(name: &str) -> bool {
    !name.is_empty() && !RESERVED_TOP_SEGMENTS.contains(&top_segment(name))
}

/// Build a JSON `Response` with the given status. Used by every `/api/*`
/// handler so payloads always advertise `application/json`.
fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn stats_to_json(s: &ApiStats) -> String {
    let KindCounts {
        image,
        audio,
        text,
        opaque,
        directory,
    } = s.objects_by_kind;
    format!(
        "{{\"nodes_total\":{nt},\
\"nodes_live\":{nl},\
\"objects_total\":{ot},\
\"objects_by_kind\":{{\"image\":{image},\"audio\":{audio},\"text\":{text},\"opaque\":{opaque},\"directory\":{directory}}},\
\"shards_total\":{sht},\
\"shards_unique\":{shu},\
\"dedup_savings_pct\":{dd:.2},\
\"bytes_total\":{bt},\
\"auto_repairs_total\":{ar},\
\"auto_repair_failures_total\":{arf},\
\"scrub_runs_total\":{srt},\
\"scrub_repairs_total\":{srpt}}}\n",
        nt = s.nodes_total,
        nl = s.nodes_live,
        ot = s.objects_total,
        sht = s.shards_total,
        shu = s.shards_unique,
        dd = s.dedup_savings_pct,
        bt = s.bytes_total,
        ar = s.auto_repairs_total,
        arf = s.auto_repair_failures_total,
        srt = s.scrub_runs_total,
        srpt = s.scrub_repairs_total,
    )
}

fn fingerprint_to_json(info: &FingerprintInfo) -> String {
    format!(
        "{{\"name\":\"{n}\",\"fingerprint\":\"{fp}\",\"kind\":\"{k}\"}}\n",
        n = json_escape(&info.name),
        fp = info.fingerprint_hex,
        k = match info.kind {
            ObjectKind::Image => "image",
            ObjectKind::Audio => "audio",
            ObjectKind::Text => "text",
            ObjectKind::Opaque => "opaque",
            ObjectKind::Directory => "directory",
        },
    )
}

/// `POST /escrow/split` — multipart `file` + `k` + `n` → in-memory shares.
/// Renders an HTML result page listing each `.holoshare` download link.
pub async fn escrow_split(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename = String::from("secret.bin");
    let mut k: usize = 3;
    let mut n: usize = 5;
    // Stage 11.19b: the form ships a hidden `lang` field carrying the
    // current page locale so the server-rendered result page matches
    // the language the user saw on `/escrow`. Falls back to `en` when
    // the field is absent or unknown.
    let mut lang = String::from("en");
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().map(|s| s.to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        match name.as_str() {
            "file" => {
                if let Some(fname) = fname {
                    if !fname.is_empty() {
                        filename = fname;
                    }
                }
                file_bytes = Some(bytes.to_vec());
            }
            "k" => {
                if let Some(v) = parse_form_usize(&bytes) {
                    k = v;
                }
            }
            "n" => {
                if let Some(v) = parse_form_usize(&bytes) {
                    n = v;
                }
            }
            "lang" => {
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    let trimmed = s.trim();
                    if crate::i18n::is_known_locale(trimmed) {
                        lang = trimmed.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    let Some(file_bytes) = file_bytes else {
        return bad_request("no file field");
    };
    match gw.escrow_split(file_bytes, filename, k, n).await {
        Ok(res) => escrow_split_html(res, &lang),
        Err(e) => error_to_response(e),
    }
}

/// `GET /escrow/download/:id_idx` — return one encoded `.holoshare`. The
/// path segment is `<escrow_id_hex>_<idx>.holoshare`; the trailing
/// `.holoshare` is stripped server-side.
pub async fn escrow_download(
    Path(path): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let stem = path.trim_end_matches(".holoshare");
    let Some((eid_hex, idx_str)) = stem.rsplit_once('_') else {
        return bad_request("bad escrow download path");
    };
    let Ok(idx) = idx_str.parse::<usize>() else {
        return bad_request("bad share index");
    };
    match gw.escrow_download(eid_hex, idx).await {
        Ok(share) => escrow_share_response(share),
        Err(GatewayError::NotFound) => (
            StatusCode::GONE,
            [(header::CONTENT_TYPE, "text/plain")],
            "escrow gone (gateway restarted — split the file again)",
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// `POST /escrow/recover` — multipart with one or more `shares=...` files.
/// Returns the recovered file with the original `Content-Type` and
/// `Content-Disposition: attachment`.
pub async fn escrow_recover(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        if name == "shares" && !bytes.is_empty() {
            blobs.push(bytes.to_vec());
        }
    }
    match gw.escrow_recover(blobs).await {
        Ok(res) => escrow_recover_response(res),
        Err(e) => error_to_response(e),
    }
}

fn escrow_split_html(res: EscrowSplitResult, lang: &str) -> Response {
    use crate::escrow::{EscrowShareRow, EscrowSplitResultView};
    use crate::i18n::{translate, LocaleSignal};
    use leptos::prelude::*;

    let EscrowSplitResult {
        filename,
        source_bytes,
        k,
        n,
        escrow_id_hex,
        shares,
    } = res;
    let rows: Vec<EscrowShareRow> = shares
        .into_iter()
        .map(|s| EscrowShareRow {
            idx: usize::from(s.idx),
            filename: s.filename,
            bytes: s.bytes as u64,
            download_path: s.download_path,
        })
        .collect();
    let lang_owned = lang.to_string();
    let lang_for_view = lang_owned.clone();
    let lang_for_shell = lang_owned.clone();

    // Stage 11.19b: render through the same `view!` pipeline the rest
    // of the site uses, then wrap in a manual document shell. We need
    // the manual shell because this response isn't routed through the
    // Leptos router — it's a direct POST result, so we can't reuse the
    // `App` shell which builds `<Router>` + `<RoutedApp>`.
    let owner = Owner::new();
    let body_html: String = owner.with(|| {
        // Provide a LocaleSignal so every `t!()` inside the view picks
        // up the request locale instead of falling back to "en".
        provide_context(LocaleSignal(Signal::derive(move || lang_for_view.clone())));
        view! {
            <EscrowSplitResultView
                filename=filename.clone()
                source_bytes=source_bytes as u64
                k=k
                n=n
                escrow_id_hex=escrow_id_hex.clone()
                shares=rows.clone()
            />
        }
        .to_html()
    });

    let title = translate("escrow.result.title_tag", &lang_for_shell);
    let body = format!(
        r#"<!DOCTYPE html><html lang="{lang_for_shell}"><head>\
<meta charset="utf-8"/>\
<meta name="viewport" content="width=device-width, initial-scale=1"/>\
<script>(function(){{try{{var t=localStorage.getItem('holofs-theme')||'dark';\
document.documentElement.dataset.theme=t;}}catch(e){{}}}})();</script>\
<link rel="stylesheet" href="/pkg/holofs.css"/>\
<title>{title}</title>\
</head><body>{body_html}</body></html>
"#,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn escrow_share_response(share: EscrowShareBytes) -> Response {
    let EscrowShareBytes { idx, total_n, bytes } = share;
    let filename = format!("share_{idx:02}_of_{total_n}.holoshare");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(bytes.into())
        .expect("share response build")
}

fn escrow_recover_response(res: EscrowRecoverResult) -> Response {
    let EscrowRecoverResult {
        data,
        content_type,
        filename,
        shares_used,
    } = res;
    let safe_filename = filename.replace('"', "");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, data.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe_filename}\""),
        )
        .header(
            HeaderName::from_static("x-holofs-escrow-shares-used"),
            shares_used as u64,
        )
        .body(data.into())
        .expect("recover response build")
}

fn bad_request_owned(msg: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain")],
        msg,
    )
        .into_response()
}

fn parse_form_usize(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

/// Minimal HTML escaper — escapes `< > & " '` for use in attributes and text nodes.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `GET /api/health/events` — Server-Sent Events stream pushing one
/// [`HealthSnapshot`] every 3 seconds. The browser's `EventSource` keeps
/// the connection open and the Leptos reactive component patches the page
/// without a full reload. Phase 4c.
pub async fn health_events(
    Extension(gw): Extension<Arc<Gateway>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        // Tick immediately so the client gets a fresh snapshot on connect,
        // then settle into a 3-second cadence.
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let data = gw.health_index_data().await;
            let snap = HealthSnapshot {
                n_live: data.n_live,
                n_total: data.n_total,
                objects: data.objects.len(),
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            };
            let event = Event::default()
                .json_data(&snap)
                .expect("HealthSnapshot must serialize");
            yield Ok::<_, Infallible>(event);
        }
    };
    // KeepAlive guards against proxies dropping idle connections.
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Escape a string for inline JSON (object names go straight into a JSON
/// response so we need to neutralise quotes and backslashes).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
