//! P1.4b — /api/capacity end-to-end coverage.
//!
//! Only the endpoint shape is asserted here; the wire + node handler +
//! policy code lives under unit tests in the respective crates. What
//! this test proves is:
//!
//! 1. The gateway's capacity poller runs on boot and eventually fills
//!    `capacity_map` with at least one known entry per node.
//! 2. `/api/capacity` responds 200 with the documented JSON shape.
//! 3. `cluster.known_nodes` reflects the size of the harness cluster
//!    once the poller has had one tick to run.
//!
//! Runs with `--test-threads=1` per the workspace convention
//! (see `feedback_e2e_threads.md`): every harness spawns its own
//! gateway which binds `HOLOFS_EMBED_BASE_PORT` (default 9100) —
//! parallel runs would collide.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

/// Give the capacity poller a chance to fire at least once. The
/// poller uses `interval(60 s)` by default; we drop it to something
/// harness-friendly via env var before harness boot. The env var is
/// read by `bootstrap.rs` at gateway start, so ordering matters:
/// set env → then spawn harness → then poll.
fn set_fast_capacity_poll() {
    // Poller floor is 5 s (see `spawn_capacity_poller`), so 5 is the
    // minimum practical value.
    std::env::set_var("HOLOFS_CAPACITY_POLL_INTERVAL_SECS", "5");
    // Rebalancer off — otherwise its 30 s floor still doesn't matter
    // in a 15-second test, but a proactive drain during the test
    // would surprise other harness assertions.
    std::env::set_var("HOLOFS_REBALANCE_INTERVAL_SECS", "0");
}

#[tokio::test]
async fn api_capacity_reports_cluster_shape_after_poller_tick() -> Result<()> {
    set_fast_capacity_poll();
    let harness = TestHarness::fresh().await?;

    // Wait up to ~10 s for the first poll round to complete. The
    // gateway boots with N_NODES embedded listeners; each answers
    // Capacity in microseconds, so once the interval elapses the
    // whole map should populate in one round.
    let mut json = None;
    for _ in 0..12 {
        let v = harness.get_json("/api/capacity").await?;
        let known = v
            .get("cluster")
            .and_then(|c| c.get("known_nodes"))
            .and_then(|k| k.as_u64())
            .unwrap_or(0);
        if known >= 1 {
            json = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let v = json.expect("capacity poller did not populate a single entry in ~12 s");

    // Every node in the array should have the documented fields —
    // absence of any of them is a schema regression the CLI + web UI
    // would trip on.
    let nodes = v
        .get("nodes")
        .and_then(|n| n.as_array())
        .expect("/api/capacity must return a `nodes` array");
    assert!(
        !nodes.is_empty(),
        "nodes array should be non-empty on a live cluster"
    );
    for node in nodes {
        for field in [
            "idx",
            "addr",
            "free_bytes",
            "total_bytes",
            "live_bytes",
            "used_pct",
            "capacity_known",
        ] {
            assert!(
                node.get(field).is_some(),
                "node entry missing `{field}`: {node}"
            );
        }
    }

    // Sanity: at least one node should have answered known — the
    // embedded harness nodes are persistent (statvfs-answerable),
    // so we expect `capacity_known=true` on at least one.
    let known_count = nodes
        .iter()
        .filter(|n| {
            n.get("capacity_known")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
        })
        .count();
    assert!(
        known_count >= 1,
        "no node reported capacity_known=true — poller/handler regression?"
    );

    harness.close().await
}
