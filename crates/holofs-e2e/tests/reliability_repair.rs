//! Regression guard for auto-repair-on-read (commit `b55e4ff`).
//!
//! The gateway's GET → decode path is wrapped with
//! `decode_with_autorepair`: on `ClientError::LayerLost` it kicks
//! `repair_object_inplace` (per-node surgical repair via
//! `list_node_hashes` + `repair_node`), persists the mutated
//! manifest, and retries the decode once.
//!
//! Counters surface via `/api/stats`:
//! `auto_repairs_total` / `auto_repair_failures_total`. The tests
//! below verify the counters move correctly under controlled
//! damage *without* the heavy-duty "kill shards on disk" mode that
//! requires write access to the harness storage dir.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn stats_endpoint_exposes_auto_repair_counters() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let stats = harness.get_json("/api/stats").await?;
    assert!(
        stats.get("auto_repairs_total").is_some(),
        "/api/stats missing `auto_repairs_total` field"
    );
    assert!(
        stats.get("auto_repair_failures_total").is_some(),
        "/api/stats missing `auto_repair_failures_total` field"
    );
    // Counters start at zero on a fresh gateway.
    let total = stats
        .get("auto_repairs_total")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(total, 0, "auto_repairs_total != 0 on a fresh gateway");
    let fails = stats
        .get("auto_repair_failures_total")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(fails, 0, "auto_repair_failures_total != 0 on a fresh gateway");
    harness.close().await
}

#[tokio::test]
async fn healthy_get_does_not_bump_repair_counters() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    // Decoding a freshly-PUT object never hits LayerLost. The
    // counter should stay at zero.
    let _ = harness
        .get_bytes("photos/abstract/mandala.png")
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stats = harness.get_json("/api/stats").await?;
    let total = stats
        .get("auto_repairs_total")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        total, 0,
        "auto_repairs_total bumped on a healthy GET — \
         `decode_with_autorepair` is triggering when it shouldn't"
    );
    harness.close().await
}

#[tokio::test]
async fn prometheus_metrics_expose_auto_repair_gauges() -> Result<()> {
    // /metrics text exposition must carry the two new counters so
    // operator dashboards can graph silent self-healing activity.
    let harness = TestHarness::fresh().await?;
    let body = String::from_utf8(harness.get_bytes("/metrics").await?).unwrap_or_default();
    for expected in [
        "holofs_auto_repairs_total",
        "holofs_auto_repair_failures_total",
    ] {
        assert!(
            body.contains(expected),
            "/metrics body missing `{expected}`. Either the handler \
             stopped emitting it or the env didn't pick up the rebuild."
        );
    }
    harness.close().await
}
