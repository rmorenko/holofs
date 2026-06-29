//! Batch C: cluster-degraded paths.
//!
//! Drives the per-node admin-kill toggle (`POST /admin/node` with
//! form field `i=<idx>`) to simulate node failures, then asserts the
//! gateway degrades gracefully — 503 on PUT to a fully-down cluster,
//! readable GETs above K, no panics on the decode path.

use anyhow::{anyhow, Result};
use holofs_e2e::TestHarness;
use reqwest::StatusCode;
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

async fn nodes_live(harness: &TestHarness) -> Result<u64> {
    let body = raw_client()
        .get(harness.url("/api/stats"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let v: Value = serde_json::from_str(&body)?;
    v["nodes_live"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing nodes_live in {body}"))
}

async fn nodes_total(harness: &TestHarness) -> Result<u64> {
    let body = raw_client()
        .get(harness.url("/api/stats"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let v: Value = serde_json::from_str(&body)?;
    v["nodes_total"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing nodes_total in {body}"))
}

/// POST /admin/node toggles node `idx`'s admin-kill flag. Returns
/// the resulting nodes_live count for the caller's bookkeeping.
async fn toggle_node(harness: &TestHarness, idx: usize) -> Result<u64> {
    let resp = raw_client()
        .post(harness.url("/admin/node"))
        .form(&[("i", idx.to_string().as_str())])
        .send()
        .await?;
    let s = resp.status();
    if !(s.is_success() || s.as_u16() == 303) {
        return Err(anyhow!("toggle_node({idx}) returned {s}"));
    }
    nodes_live(harness).await
}

/// Kill every node in the cluster. Convenience for the "fully-down"
/// scenarios — the gateway's effective_live becomes empty.
async fn kill_all_nodes(harness: &TestHarness) -> Result<()> {
    let n = nodes_total(harness).await? as usize;
    for i in 0..n {
        toggle_node(harness, i).await?;
    }
    let live = nodes_live(harness).await?;
    assert_eq!(live, 0, "expected 0 live nodes after kill_all, got {live}");
    Ok(())
}

// === PUT into degraded cluster ============================================

/// Previously this path panicked inside `place_shard` ("live node set
/// is empty"). Phase 5 / session-15.x made it a typed `NoLiveNodes`
/// → `GatewayError::ClusterDegraded` → 503. This test pins that
/// contract — fully-down cluster must NEVER 5xx with a panic.
#[tokio::test]
async fn put_to_fully_down_cluster_returns_503() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    kill_all_nodes(&harness).await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/dead-cluster.png"))
        .body(body)
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "PUT to fully-down cluster must surface as 503"
    );
    let msg = resp.text().await.unwrap_or_default();
    assert!(
        msg.to_lowercase().contains("cluster") || msg.to_lowercase().contains("live"),
        "503 body should reference the cluster-degraded reason, got {msg:?}"
    );
    harness.close().await
}

/// Survivable degradation: kill a *small* subset of nodes — well below
/// K — and verify a fresh PUT/GET round-trip still works. Catches
/// regressions where the gateway treats *any* node loss as fatal.
#[tokio::test]
async fn put_and_get_with_partial_node_loss_still_works() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    // Kill 4 nodes (10% of a 40-node default cluster) — well under K=16.
    let total = nodes_total(&harness).await?;
    let to_kill = ((total / 10).max(1) as usize).min(4);
    for i in 0..to_kill {
        toggle_node(&harness, i).await?;
    }
    assert!(
        nodes_live(&harness).await? < total,
        "kill should have lowered nodes_live"
    );
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/degraded.png", body).await?;
    let bytes = harness.get_bytes("photos/degraded.png").await?;
    assert!(
        bytes.len() > 100,
        "GET under partial degradation returned {} bytes",
        bytes.len()
    );
    harness.close().await
}

// === Recovery =============================================================

/// Cluster recovery: kill all nodes, PUT fails 503, un-kill all,
/// retry the PUT, it succeeds. Pins the "503 is transient" contract —
/// a client that retries after the cluster comes back must not see
/// stale state.
#[tokio::test]
async fn cluster_recovers_after_unkilling_all_nodes() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    kill_all_nodes(&harness).await?;
    // First PUT fails 503.
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/recover.png"))
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Un-kill every node. toggle is a flip, so a second toggle of
    // the same idx restores it.
    let n = nodes_total(&harness).await? as usize;
    for i in 0..n {
        toggle_node(&harness, i).await?;
    }
    assert_eq!(nodes_live(&harness).await?, n as u64);

    // Retry — should succeed.
    let resp = raw_client()
        .put(harness.url("photos/recover.png"))
        .body(body)
        .send()
        .await?;
    assert!(
        resp.status().is_success(),
        "PUT after recovery: expected 2xx, got {}",
        resp.status()
    );
    let bytes = harness.get_bytes("photos/recover.png").await?;
    assert!(bytes.len() > 100);
    harness.close().await
}

// === /admin/node form-handling ============================================

/// /admin/node with missing or non-numeric `i` → 400. Catches handler-
/// level form parsing regressions that could send arbitrary bytes to
/// `toggle_admin_kill`.
#[tokio::test]
async fn admin_node_missing_field_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .post(harness.url("/admin/node"))
        .form(&[("not_i", "0")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = raw_client()
        .post(harness.url("/admin/node"))
        .form(&[("i", "not-a-number")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

/// Toggling node 0 lowers nodes_live by 1; toggling it again brings
/// it back. Round-trip invariant on the admin-kill flag.
#[tokio::test]
async fn admin_node_toggle_round_trips_live_count() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let baseline = nodes_live(&harness).await?;
    let after_kill = toggle_node(&harness, 0).await?;
    assert_eq!(after_kill, baseline - 1);
    let after_revive = toggle_node(&harness, 0).await?;
    assert_eq!(after_revive, baseline);
    harness.close().await
}
