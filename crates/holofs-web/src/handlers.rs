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
use axum::extract::{Extension, Multipart, Path};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;
use std::convert::Infallible;
use std::time::Duration;

use holofs_gateway::{FingerprintInfo, Gateway, GatewayError};
use holofs_model::manifest::ObjectKind;

use crate::health::HealthSnapshot;

// R2b.1: escrow handlers (POST /escrow/split, GET /escrow/download,
// POST /escrow/recover) live in a submodule. `pub use` keeps the
// historical `handlers::escrow_split` / `handlers::escrow_download`
// / `handlers::escrow_recover` paths resolving in main.rs.
mod escrow;
pub use escrow::{escrow_download, escrow_recover, escrow_split};

// R2b.2: pure helpers + response builders split off. Their
// pub(crate) items are re-exported here so this file (and the
// escrow submodule) can `use super::bad_request` etc. — nothing
// changes at the call sites.
pub(crate) mod response;
pub(crate) mod util;
pub(crate) use response::{
    decoded_to_response, fingerprint_to_json, ingest_to_response, mkdir_to_response,
    partial_response, remove_to_response, rename_to_response, rmdir_to_response,
    serve_with_range, stats_to_json, unsatisfiable_response,
};
pub(crate) use util::{
    bad_request, bad_request_owned, error_to_response, is_reserved_name, is_valid_put_name,
    json_escape, json_response, kind_label, not_found, parse_urlencoded_field, pick_return_to,
    redirect_to, render_shard_as_png, top_segment, url_decode_simple, url_encode_simple, x,
};

// R2b.3: seven object-CRUD handlers moved to a domain module.
// `pub use` keeps the historical `handlers::get_object` etc.
// paths resolving from main.rs.
mod objects;
pub use objects::{
    delete_object, get_object, get_preview, get_shard_png, preview_stream, put_object,
    serve_wasm_alias,
};






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

    // N8: reliability counters wired up by the middleware stack.
    // Everything here is `Arc<AtomicU64>` on the Gateway; a single
    // ObservabilityCounters borrow gives us Relaxed reads without
    // holding any lock.
    use std::sync::atomic::Ordering;
    let obs = gw.observability_counters();

    body.push_str("# HELP holofs_catalog_persist_failures_total Atomic catalog save-to-disk failures. Non-zero means the on-disk catalog is behind memory.\n");
    body.push_str("# TYPE holofs_catalog_persist_failures_total counter\n");
    body.push_str(&format!(
        "holofs_catalog_persist_failures_total {}\n",
        obs.catalog_persist_failures_total.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_handler_timeouts_total 504 Gateway Timeout responses, split by deadline bucket.\n");
    body.push_str("# TYPE holofs_handler_timeouts_total counter\n");
    body.push_str(&format!(
        "holofs_handler_timeouts_total{{bucket=\"short\"}} {}\n",
        obs.timeout_short_total.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_handler_timeouts_total{{bucket=\"medium\"}} {}\n",
        obs.timeout_medium_total.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_handler_timeouts_total{{bucket=\"long\"}} {}\n",
        obs.timeout_long_total.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_backpressure_rejected_total 503 responses caused by N3 backpressure (semaphore at capacity), split by bucket.\n");
    body.push_str("# TYPE holofs_backpressure_rejected_total counter\n");
    body.push_str(&format!(
        "holofs_backpressure_rejected_total{{bucket=\"medium\"}} {}\n",
        obs.medium_rejected_total.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_backpressure_rejected_total{{bucket=\"long\"}} {}\n",
        obs.long_rejected_total.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_backpressure_permits_available Backpressure semaphore permits still free, by bucket.\n");
    body.push_str("# TYPE holofs_backpressure_permits_available gauge\n");
    body.push_str(&format!(
        "holofs_backpressure_permits_available{{bucket=\"medium\"}} {}\n",
        obs.medium_permits.available_permits()
    ));
    body.push_str(&format!(
        "holofs_backpressure_permits_available{{bucket=\"long\"}} {}\n",
        obs.long_permits.available_permits()
    ));

    body.push_str("# HELP holofs_supervised_task_restarts_total Supervised background task restarts (panic or unexpected exit). Non-zero flags a repeated crash under monitor/auditor/scrub.\n");
    body.push_str("# TYPE holofs_supervised_task_restarts_total counter\n");
    body.push_str(&format!(
        "holofs_supervised_task_restarts_total{{task=\"monitor\"}} {}\n",
        obs.task_restarts_monitor.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_supervised_task_restarts_total{{task=\"auditor\"}} {}\n",
        obs.task_restarts_auditor.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_supervised_task_restarts_total{{task=\"scrub\"}} {}\n",
        obs.task_restarts_scrub.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_admin_auth_failures_total N6 admin bearer-token rejections, split by reason.\n");
    body.push_str("# TYPE holofs_admin_auth_failures_total counter\n");
    body.push_str(&format!(
        "holofs_admin_auth_failures_total{{outcome=\"missing\"}} {}\n",
        obs.admin_auth_missing_total.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_admin_auth_failures_total{{outcome=\"bad\"}} {}\n",
        obs.admin_auth_bad_total.load(Ordering::Relaxed)
    ));
    body.push_str(&format!(
        "holofs_admin_auth_failures_total{{outcome=\"disabled\"}} {}\n",
        obs.admin_auth_disabled_total.load(Ordering::Relaxed)
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

/// `POST /api/versions/delete` — form-friendly version deletion.
/// Body: `name=<path>&id=<version_id>&return_to=<url>`. On success
/// 303-redirects to `return_to` (defaults to `/versions/<name>`).
/// The deleted version's uniquely-owned shards are GC'd from the
/// cluster in the same call.
pub async fn delete_version_form(
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
    match gw.delete_version(&name, &id).await {
        Ok(_) => redirect_to(&return_to),
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

