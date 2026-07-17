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

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Extension, Query};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;

use holofs_gateway::Gateway;

use crate::audit::{log_event, AuditEvent};
use crate::health::HealthSnapshot;

use super::response::stats_to_json;
use super::util::{error_to_response, json_escape, json_response, parse_urlencoded_field};

/// `GET /api/stats` — JSON snapshot of cluster-wide counters.
pub async fn api_stats(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    json_response(StatusCode::OK, stats_to_json(&gw.api_stats().await))
}

/// `GET /api/capacity` — P1.4b JSON snapshot of the per-node disk
/// footprint cached by the capacity poller. One entry per known node
/// (a node the poller has heard back from at least once). Nodes with
/// `total_bytes = 0` are reported as `capacity_known=false` so the
/// operator UI can grey them out instead of showing 0% used.
///
/// Response shape:
///
/// ```json
/// {
///   "nodes": [
///     {"idx":0,"addr":"127.0.0.1:9101","free_bytes":123,"total_bytes":456,
///      "live_bytes":78,"used_pct":17.11,"capacity_known":true,
///      "age_secs":42},
///     …
///   ],
///   "cluster": {
///     "known_nodes": 3,
///     "min_used_pct": 12.5,
///     "max_used_pct": 71.2,
///     "skew_ratio": 5.7
///   }
/// }
/// ```
///
/// `skew_ratio = max_used / max(min_used, 1)` — matches the WARN
/// threshold in the capacity poller. Absent (null) when fewer than
/// two nodes report known capacity.
pub async fn api_capacity(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    let snap = holofs_gateway::capacity::snapshot(&gw.capacity_map).await;
    let cluster = gw.cluster();
    let now = std::time::Instant::now();
    let mut body = String::with_capacity(256 + cluster.node_addrs.len() * 160);
    body.push_str("{\"nodes\":[");
    let mut first = true;
    for (idx, addr) in cluster.node_addrs.iter().enumerate() {
        if !first {
            body.push(',');
        }
        first = false;
        match snap.get(addr) {
            Some(entry) => {
                let age = now.saturating_duration_since(entry.updated_at).as_secs();
                let known = entry.capacity.is_known();
                let used_pct = entry.capacity.used_pct();
                body.push_str(&format!(
                    "{{\"idx\":{idx},\"addr\":\"{}\",\"free_bytes\":{},\
                     \"total_bytes\":{},\"live_bytes\":{},\"used_pct\":{:.2},\
                     \"capacity_known\":{},\"age_secs\":{}}}",
                    addr.replace('"', "\\\""),
                    entry.capacity.free_bytes,
                    entry.capacity.total_bytes,
                    entry.capacity.live_bytes,
                    used_pct,
                    known,
                    age,
                ));
            }
            None => {
                // Never heard back — surface with null-ish sentinel
                // so the client can distinguish "poll hasn't run yet"
                // from "node reported (0, 0)".
                body.push_str(&format!(
                    "{{\"idx\":{idx},\"addr\":\"{}\",\"free_bytes\":0,\
                     \"total_bytes\":0,\"live_bytes\":0,\"used_pct\":0.00,\
                     \"capacity_known\":false,\"age_secs\":null}}",
                    addr.replace('"', "\\\"")
                ));
            }
        }
    }
    body.push(']');
    // Aggregate skew stats. Snapshot the values once and reuse; the
    // helper takes a slice, so a small clone-out is cheaper than
    // locking the map again.
    let entries: Vec<_> = snap.values().copied().collect();
    let (known_nodes, cluster_frag) = {
        let n_known = entries.iter().filter(|e| e.capacity.is_known()).count();
        let cluster_line = match holofs_gateway::capacity::min_max_used_pct(&entries) {
            Some((mn, mx)) => format!(
                "\"min_used_pct\":{:.2},\"max_used_pct\":{:.2},\"skew_ratio\":{:.2}",
                mn,
                mx,
                mx / mn.max(1.0),
            ),
            None => "\"min_used_pct\":null,\"max_used_pct\":null,\"skew_ratio\":null".into(),
        };
        (n_known, cluster_line)
    };
    body.push_str(&format!(
        ",\"cluster\":{{\"known_nodes\":{known_nodes},{cluster_frag}}}}}"
    ));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
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

    body.push_str(
        "# HELP holofs_shards_total Planned shards across every object × layer × channel.\n",
    );
    body.push_str("# TYPE holofs_shards_total gauge\n");
    body.push_str(&format!("holofs_shards_total {}\n", stats.shards_total));

    body.push_str(
        "# HELP holofs_shards_unique Distinct shard hashes recorded across the catalog.\n",
    );
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
    body.push_str(&format!(
        "holofs_scrub_runs_total {}\n",
        stats.scrub_runs_total
    ));

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
            log_event(
                &AuditEvent::now("gc_orphans", "", "ok").with_details(format!(
                    "{{\"live_hashes\":{},\"purged_total\":{},\"duration_ms\":{}}}",
                    rep.live_hashes, rep.purged_total, rep.duration_ms,
                )),
            );
            (
                StatusCode::OK,
                [(http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Err(e) => {
            log_event(&AuditEvent::now("gc_orphans", "", "error"));
            error_to_response(e)
        }
    }
}

/// `POST /admin/node` — toggle admin-kill for node `i` (form field).
/// Used by the kill/revive buttons on `/health`; redirects back to
/// `/health` (303).
pub async fn toggle_node(Extension(gw): Extension<Arc<Gateway>>, body: Bytes) -> Response {
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
        Ok(_) => {
            log_event(&AuditEvent::now("toggle_node", format!("idx={idx}"), "ok"));
            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/health")
                .body(axum::body::Body::empty())
                .expect("redirect build")
        }
        Err(e) => {
            log_event(&AuditEvent::now(
                "toggle_node",
                format!("idx={idx}"),
                "error",
            ));
            error_to_response(e)
        }
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
pub async fn admin_add_node(Extension(gw): Extension<Arc<Gateway>>, body: Bytes) -> Response {
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
    log_event(
        &AuditEvent::now(
            "add_node",
            format!("addr={addr},zone={zone}"),
            if failed > 0 { "error" } else { "ok" },
        )
        .with_details(format!(
            "{{\"objects_rebalanced\":{},\"objects_failed\":{}}}",
            ok, failed
        )),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// `POST /admin/drain_node` — flip `admin_kills[idx] = true` and
/// rebalance every catalog entry so shards HRW-assigned to `idx`
/// land on live neighbours. Optional `purge=true` sends
/// `Request::PurgeByHash` for every hash on the drained node after
/// a successful sweep, reclaiming disk before physical
/// decommission.
///
/// Form body: `idx=N` (required) `purge=true|false` (default false).
/// Response: JSON `{ idx, drained_objects, failed_objects,
///                   purged, error? }`.
///
/// Admin-token gated. Operators typically run this on a maintenance
/// window and follow with a whitelist re-sign that omits the drained
/// node, then hot-reload — see operations.md §10.
pub async fn admin_drain_node(Extension(gw): Extension<Arc<Gateway>>, body: Bytes) -> Response {
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
    let idx_str = parse_urlencoded_field(body_str, "idx").unwrap_or_default();
    let purge_str = parse_urlencoded_field(body_str, "purge").unwrap_or_default();
    let idx: usize = match idx_str.parse() {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "text/plain")],
                "missing or invalid field idx",
            )
                .into_response();
        }
    };
    let purge = matches!(purge_str.as_str(), "true" | "1" | "yes");

    let outcome = gw.drain_node(idx, purge).await;
    let ok = outcome.reports.iter().filter(|r| r.result.is_ok()).count();
    let failed = outcome.reports.iter().filter(|r| r.result.is_err()).count();
    let error_frag = match &outcome.error {
        Some(e) => format!(",\"error\":\"{}\"", json_escape(e)),
        None => String::new(),
    };
    let body = format!(
        "{{\"idx\":{},\"admin_kill_set\":{},\"drained_objects\":{},\
         \"failed_objects\":{},\"purged\":{},\
         \"warning\":\"admin_kills is a runtime-only flag — restart the gateway with a whitelist that omits the drained node to make removal permanent\"{}}}",
        idx, outcome.admin_kill_set, ok, failed, outcome.purged, error_frag,
    );
    log_event(
        &AuditEvent::now(
            "drain_node",
            format!("idx={idx}"),
            if outcome.error.is_some() || failed > 0 {
                "error"
            } else {
                "ok"
            },
        )
        .with_details(format!(
            "{{\"admin_kill_set\":{},\"drained_objects\":{},\"failed_objects\":{},\"purged\":{}}}",
            outcome.admin_kill_set, ok, failed, outcome.purged,
        )),
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
/// # Response shapes (backward-compatible)
///
/// - **No query params** → bare array of every entry, as before.
///   Preserves the shape older `holofs-admin` binaries expect.
///
///   ```json
///   [
///     {"name": "photos/beach.jpg", "kind": "image", "size": 4194304},
///     {"name": "docs/notes.txt",   "kind": "text",  "size": 1024}
///   ]
///   ```
///
/// - **With `?cursor=&limit=`** → paginated wrapper. `cursor` is an
///   opaque, URL-safe base64 string returned as `next_cursor` on the
///   previous page (empty / omitted on the first page). `limit`
///   defaults to `DEFAULT_PAGE_LIMIT` and is clamped to
///   `MAX_PAGE_LIMIT`.
///
///   ```json
///   {
///     "items": [
///       {"name": "photos/beach.jpg", "kind": "image", "size": 4194304}
///     ],
///     "next_cursor": "ZG9jcy9ub3Rlcy50eHQ"
///   }
///   ```
///
///   `next_cursor` is `null` when the walk hit the end of the
///   catalog. Callers iterate until they see `null`, not until
///   `items` is empty (a page falling entirely on skipped directory
///   manifests can hand back `items=[]` with a non-null cursor).
pub async fn admin_catalog_names(
    Extension(gw): Extension<Arc<Gateway>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Presence of *any* pagination-shaped query param opts into the
    // wrapper response. Absence keeps the pre-P2.1 bare-array shape
    // so unchanged clients (older `holofs-admin`, ad-hoc curl
    // scripts) don't get a schema surprise.
    let paginated = params.contains_key("cursor") || params.contains_key("limit");

    if !paginated {
        let entries = gw.list_all_objects().await;
        let n = entries.len();
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
        log_event(
            &AuditEvent::now("catalog_names", "", "ok")
                .with_details(format!("{{\"objects\":{n},\"paginated\":false}}")),
        );
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            buf,
        )
            .into_response();
    }

    // --- Paginated branch. ---
    let limit_raw: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .max(1);
    let limit = limit_raw.min(MAX_PAGE_LIMIT);

    // Decode cursor. Empty string / absent → start from beginning.
    // Invalid base64 or non-UTF8 → 400.
    let after_owned: Option<String> = match params.get("cursor").map(String::as_str) {
        None | Some("") => None,
        Some(raw) => match decode_cursor(raw) {
            Ok(name) => Some(name),
            Err(msg) => {
                log_event(&AuditEvent::now("catalog_names", "", "error"));
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "text/plain")],
                    format!("bad cursor: {msg}"),
                )
                    .into_response();
            }
        },
    };
    let after: Option<&str> = after_owned.as_deref();

    let page = gw.list_all_objects_paginated(after, limit).await;
    let n = page.items.len();
    let mut buf = String::from("{\"items\":[");
    for (i, e) in page.items.iter().enumerate() {
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
    buf.push_str("],\"next_cursor\":");
    match &page.next_cursor {
        Some(name) => {
            let enc = encode_cursor(name);
            buf.push('"');
            buf.push_str(&enc);
            buf.push('"');
        }
        None => buf.push_str("null"),
    }
    buf.push('}');
    log_event(
        &AuditEvent::now("catalog_names", "", "ok").with_details(format!(
            "{{\"objects\":{n},\"paginated\":true,\"limit\":{limit},\"has_next\":{}}}",
            page.next_cursor.is_some()
        )),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        buf,
    )
        .into_response()
}

/// Default page size when `?limit=` is omitted. Matches the memory
/// footprint of a ~200-byte-per-entry JSON row × 1000 = ~200 KiB,
/// which is safe for admin operators over a home network.
const DEFAULT_PAGE_LIMIT: usize = 1000;

/// Hard ceiling on `?limit=`. Prevents an operator typo (`limit=9999999`)
/// from making the gateway allocate megabytes for a single response.
const MAX_PAGE_LIMIT: usize = 10_000;

/// URL-safe base64 (no padding) encoder for the cursor. Cursor
/// content is the last catalog name we returned; base64 shields it
/// from URL-encoding surprises (`/` in path-like names, `=` in tag
/// syntax) without pulling a new dep.
fn encode_cursor(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        out.push(A[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        out.push(A[(b2 & 0b111111) as usize] as char);
        i += 3;
    }
    match bytes.len() - i {
        1 => {
            let b0 = bytes[i];
            out.push(A[(b0 >> 2) as usize] as char);
            out.push(A[((b0 & 0b11) << 4) as usize] as char);
        }
        2 => {
            let b0 = bytes[i];
            let b1 = bytes[i + 1];
            out.push(A[(b0 >> 2) as usize] as char);
            out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
            out.push(A[((b1 & 0b1111) << 2) as usize] as char);
        }
        _ => {}
    }
    out
}

/// Inverse of [`encode_cursor`]. Returns the decoded name on success
/// or a descriptive error message on any malformed input.
fn decode_cursor(s: &str) -> Result<String, String> {
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let src = s.as_bytes();
    let mut out = Vec::with_capacity(src.len() * 3 / 4);
    let mut i = 0;
    while i + 4 <= src.len() {
        let d = |k: usize| digit(src[k]).ok_or_else(|| format!("bad char at {k}"));
        let d0 = d(i)?;
        let d1 = d(i + 1)?;
        let d2 = d(i + 2)?;
        let d3 = d(i + 3)?;
        out.push((d0 << 2) | (d1 >> 4));
        out.push((d1 << 4) | (d2 >> 2));
        out.push((d2 << 6) | d3);
        i += 4;
    }
    match src.len() - i {
        0 => {}
        1 => return Err("truncated cursor (single trailing char)".into()),
        2 => {
            let d0 = digit(src[i]).ok_or_else(|| format!("bad char at {i}"))?;
            let d1 = digit(src[i + 1]).ok_or_else(|| format!("bad char at {}", i + 1))?;
            out.push((d0 << 2) | (d1 >> 4));
        }
        3 => {
            let d0 = digit(src[i]).ok_or_else(|| format!("bad char at {i}"))?;
            let d1 = digit(src[i + 1]).ok_or_else(|| format!("bad char at {}", i + 1))?;
            let d2 = digit(src[i + 2]).ok_or_else(|| format!("bad char at {}", i + 2))?;
            out.push((d0 << 2) | (d1 >> 4));
            out.push((d1 << 4) | (d2 >> 2));
        }
        _ => unreachable!(),
    }
    String::from_utf8(out).map_err(|e| format!("cursor is not utf-8: {e}"))
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

#[cfg(test)]
mod tests {
    use super::{decode_cursor, encode_cursor};

    #[test]
    fn cursor_roundtrip_short_name() {
        // Every input length mod 3 (0, 1, 2) exercises a different
        // tail-padding branch in the encoder, so cover all three.
        for name in &["", "a", "ab", "abc", "abcd", "abcde", "abcdef"] {
            let enc = encode_cursor(name);
            let dec = decode_cursor(&enc).expect("must round-trip");
            assert_eq!(&dec, *name, "roundtrip failure for {name:?} (enc={enc})");
        }
    }

    #[test]
    fn cursor_roundtrip_paths_with_slashes_and_dots() {
        // Catalog names look like `photos/2026/beach.jpg` — the
        // `/`, `.`, and other URL-unfriendly chars must survive
        // encode+decode without any URL-percent-encoding contortions.
        for name in &[
            "photos/2026/beach.jpg",
            "docs/notes.txt",
            "a/b/c/d/e/f/g",
            "spaces are here.txt",
            "utf8: файл.дат",
            "..\\weird_win.bin",
        ] {
            let enc = encode_cursor(name);
            let dec = decode_cursor(&enc).expect("must round-trip");
            assert_eq!(&dec, *name);
        }
    }

    #[test]
    fn cursor_produces_only_url_safe_chars() {
        // Base64 URL-safe alphabet: A-Za-z0-9-_ (no `+`, `/`, `=`).
        // If any encoder branch leaked a stock-base64 char we'd need
        // percent-encoding at HTTP layer — the whole point of the
        // URL-safe variant is that we don't.
        for name in &["path/with/slash", "abc123", "🙂"] {
            let enc = encode_cursor(name);
            for b in enc.bytes() {
                assert!(
                    b.is_ascii_alphanumeric() || b == b'-' || b == b'_',
                    "encoded cursor {enc} contains non-URL-safe byte {b:#x}"
                );
            }
        }
    }

    #[test]
    fn decode_cursor_rejects_malformed_input() {
        assert!(decode_cursor("!!!").is_err(), "non-base64 chars → error");
        assert!(decode_cursor("A").is_err(), "single-char tail is invalid");
    }
}
