//! Cluster health + observability axum handlers.
//!
//! Five routes wrapping the [`Gateway`] stats + admin surface:
//!
//! - `GET /api/stats` — [`api_stats`] JSON snapshot.
//! - `GET /metrics` — [`metrics`] Prometheus text exposition.
//! - `POST /api/gc` — [`gc_orphans`] orphan-shard sweep.
//! - `POST /admin/node` — [`toggle_node`] admin-kill flip (N6-gated
//!   by the middleware layer in `main.rs`).
//! - `GET /api/health/events` — [`health_events`] Server-Sent Events
//!   stream of [`crate::health::HealthSnapshot`] every 3 s.
//!

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;

use holofs_gateway::Gateway;

use crate::health::HealthSnapshot;

use super::response::stats_to_json;
use super::util::{error_to_response, json_escape, json_response, parse_urlencoded_field};

/// `GET /api/stats` — JSON snapshot of cluster-wide counters.
pub async fn api_stats(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    json_response(StatusCode::OK, stats_to_json(&gw.api_stats().await))
}

/// `GET /metrics` — Prometheus exposition format (text-version 0.0.4).
/// Pull-based gauges sourced from [`Gateway::api_stats`] + the
/// per-node admin-kill snapshot + the N-series counters exposed via
/// [`Gateway::observability_counters`]. Production deployments scrape
/// this every ~15s.
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

    body.push_str("# HELP holofs_objects_encoding Currently-encoding objects (async ingest, in-flight background workers).\n");
    body.push_str("# TYPE holofs_objects_encoding gauge\n");
    body.push_str(&format!(
        "holofs_objects_encoding {}\n",
        obs.objects_encoding.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_encode_completed_total Async-ingest background encodes that finished successfully.\n");
    body.push_str("# TYPE holofs_encode_completed_total counter\n");
    body.push_str(&format!(
        "holofs_encode_completed_total {}\n",
        obs.encode_completed_total.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_encode_failed_total Async-ingest background encodes that failed (encode error, cluster degraded, catalog persist error). Manifest gets state=Failed.\n");
    body.push_str("# TYPE holofs_encode_failed_total counter\n");
    body.push_str(&format!(
        "holofs_encode_failed_total {}\n",
        obs.encode_failed_total.load(Ordering::Relaxed)
    ));

    // Cpu-vs-fanout split for `put_object`. Two atomics/PUT — the
    // previous attempt at file-based tracing (`/tmp/encode_timing.log`)
    // stole ~14% of throughput to lock contention, which corrupted
    // the very numbers it was trying to measure.
    body.push_str("# HELP holofs_put_cpu_nanoseconds_sum Sum of CPU-phase (RLNC + DWT + hash) nanoseconds inside put_object.\n");
    body.push_str("# TYPE holofs_put_cpu_nanoseconds_sum counter\n");
    body.push_str(&format!(
        "holofs_put_cpu_nanoseconds_sum {}\n",
        holofs_client::PUT_CPU_NS_SUM.load(Ordering::Relaxed)
    ));
    body.push_str("# HELP holofs_put_fanout_nanoseconds_sum Sum of fanout-phase (network) nanoseconds inside put_object.\n");
    body.push_str("# TYPE holofs_put_fanout_nanoseconds_sum counter\n");
    body.push_str(&format!(
        "holofs_put_fanout_nanoseconds_sum {}\n",
        holofs_client::PUT_FANOUT_NS_SUM.load(Ordering::Relaxed)
    ));
    body.push_str("# HELP holofs_put_count_total Number of put_object completions contributing to CPU / fanout ns sums.\n");
    body.push_str("# TYPE holofs_put_count_total counter\n");
    body.push_str(&format!(
        "holofs_put_count_total {}\n",
        holofs_client::PUT_COUNT.load(Ordering::Relaxed)
    ));

    body.push_str("# HELP holofs_rate_limit_rejected_total v0.7 per-IP rate limit rejections (429 responses).\n");
    body.push_str("# TYPE holofs_rate_limit_rejected_total counter\n");
    body.push_str(&format!(
        "holofs_rate_limit_rejected_total {}\n",
        obs.rate_limit_rejected_total.load(Ordering::Relaxed)
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

/// `POST /api/gc` — sweep orphan shards from every live
/// cluster node. Returns the per-node breakdown as JSON.
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

/// `POST /admin/node` — toggle admin-kill for node `i` (form field).
/// Used by the kill/revive buttons on `/health`; redirects back to
/// `/health` (303).
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

/// v3-8: `POST /admin/add_node` — admin-triggered per-object
/// rebalance. Form body: `addr=<host:port>&zone=<u8>`. Runs
/// `Gateway::rebalance_add_node` so every catalog manifest's
/// `nodes` table grows to include the new address and its HRW share
/// of shards is fanned out via `repair_node`. Returns a JSON summary
/// listing per-object success/error plus a diagnostic reminder that
/// the gateway's own `ClusterInfo.node_addrs` still needs a restart
/// with the new whitelist to bring the cluster topology fully into
/// agreement.
pub async fn admin_add_node(
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
    let addr = parse_urlencoded_field(body_str, "addr").unwrap_or_default();
    let zone_str = parse_urlencoded_field(body_str, "zone").unwrap_or_default();
    if addr.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain")],
            "missing field addr",
        )
            .into_response();
    }
    let zone: u8 = zone_str.parse().unwrap_or(0);

    let reports = gw.rebalance_add_node(addr.clone(), zone).await;
    let ok = reports.iter().filter(|r| r.result.is_ok()).count();
    let failed = reports.iter().filter(|r| r.result.is_err()).count();
    let body = format!(
        "{{\"addr\":\"{}\",\"zone\":{},\
         \"objects_rebalanced\":{},\"objects_failed\":{},\
         \"warning\":\"gateway ClusterInfo.node_addrs unchanged — restart the gateway with the new whitelist to bring the cluster topology into agreement\"}}",
        addr.replace('"', "\\\""),
        zone,
        ok,
        failed
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// `GET /admin/catalog_names` — flat enumeration of every
/// non-directory catalog entry, JSON-encoded. Consumed by
/// `holofs-admin export-all` to drive per-object HTTP GETs against
/// the gateway. Admin-token gated because a full catalog listing is
/// operationally sensitive (reveals object namespaces and sizes even
/// if the objects themselves are gated elsewhere).
///
/// Response shape:
///
/// ```json
/// [
///   {"name": "photos/beach.jpg", "kind": "image", "size": 4194304},
///   {"name": "docs/notes.txt",   "kind": "text",  "size": 1024},
///   …
/// ]
/// ```
pub async fn admin_catalog_names(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    let entries = gw.list_all_objects().await;
    let mut buf = String::from("[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        buf.push_str(&format!(
            "{{\"name\":\"{}\",\"kind\":\"{}\",\"size\":{}}}",
            json_escape(&e.name),
            object_kind_json_label(e.kind),
            e.size,
        ));
    }
    buf.push(']');
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        buf,
    )
        .into_response()
}

fn object_kind_json_label(kind: holofs_model::manifest::ObjectKind) -> &'static str {
    use holofs_model::manifest::ObjectKind;
    match kind {
        ObjectKind::Image => "image",
        ObjectKind::Text => "text",
        ObjectKind::Audio => "audio",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
}

/// `GET /api/health/events` — Server-Sent Events stream pushing one
/// [`HealthSnapshot`] every 3 seconds. The browser's `EventSource`
/// keeps the connection open and the Leptos reactive component
/// patches the page without a full reload. Phase 4c.
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
                ts_ms: holofs_core::time::now_unix_ms(),
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
