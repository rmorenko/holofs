//! v2 review round #2b — auto-rebalance daemon triggers when
//! `capacity_map` shows a real physical skew.
//!
//! The `spawn_auto_rebalancer` background loop is disabled in e2e
//! (`HOLOFS_REBALANCE_INTERVAL_SECS=0` in `capacity_endpoint.rs`)
//! because it takes minutes to fire on the default interval. This
//! test drives one tick synchronously via the extracted
//! [`holofs_gateway::capacity::rebalance_tick`] helper, so the
//! decision + admin_kill flip + drain invocation are all exercised
//! deterministically.
//!
//! The cluster has no real network nodes (bare `Gateway::new` with
//! empty `node_addrs` in `LiveNodes`) — the catalog is empty so
//! `drain_node` returns zero migrations, which is fine. What the
//! test verifies is that the tick actually *fires* on the correct
//! decision when capacity is skewed, and correctly returns `None`
//! when it isn't. The plumbing between decision → drain call is
//! what unit-testing `decide_rebalance` in isolation didn't
//! previously cover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use holofs_client::{LiveNodes, NodeCapacity};
use holofs_core::gf::Gf;
use holofs_gateway::capacity::{empty_map, rebalance_tick, CapacityEntry};
use holofs_gateway::{ClusterInfo, Gateway};
use holofs_model::fs::Directory;
use holofs_model::placement::Placement;

/// Build a Gateway that has real node addresses (so
/// `capacity_map` lookups by addr resolve) but no live network
/// nodes behind them. Perfect for exercising the rebalance policy
/// path without needing a real cluster.
fn build_gateway_with_addrs(addrs: Vec<String>) -> Arc<Gateway> {
    let n = addrs.len();
    let gf = Arc::new(Gf::new());
    let catalog = Arc::new(RwLock::new(Directory::new()));
    let live: Arc<LiveNodes> = Arc::new((0..n).collect());
    let cluster = Arc::new(ClusterInfo {
        node_addrs: addrs,
        zones: vec![0; n],
        placement: Placement::Rendezvous,
        width: 0,
        height: 0,
    });
    Gateway::new(gf, catalog, live, cluster)
}

/// Insert a capacity entry directly into the gateway's `capacity_map`
/// as if the poller had just heard back from that node. Bypasses the
/// wire path entirely — the whole point of this test is the
/// rebalance decision, not the polling round.
async fn inject_capacity(
    map: &holofs_gateway::capacity::CapacityMap,
    addr: &str,
    free: u64,
    total: u64,
    live: u64,
) {
    let mut g = map.lock().await;
    g.insert(
        addr.to_string(),
        CapacityEntry {
            capacity: NodeCapacity {
                free_bytes: free,
                total_bytes: total,
                live_bytes: live,
            },
            updated_at: Instant::now(),
        },
    );
}

#[tokio::test]
async fn rebalance_tick_fires_on_injected_physical_skew() {
    // Two nodes: N0 at physical 90 %, N1 at physical 5 %. With
    // trigger=85 and cold ceiling=60, the tick MUST return
    // Some(outcome) with decision.drain_addr_idx = 0.
    let gw = build_gateway_with_addrs(vec!["node0".into(), "node1".into()]);
    let map = empty_map();
    inject_capacity(&map, "node0", 10, 100, 90).await;
    inject_capacity(&map, "node1", 95, 100, 5).await;

    let catalog: Arc<RwLock<Directory>> = Arc::new(RwLock::new(Directory::new()));
    let outcome = rebalance_tick(&gw, &map, &catalog, 85.0, 60.0, 4)
        .await
        .expect("physical skew above trigger MUST produce a tick outcome");
    assert_eq!(
        outcome.decision.drain_addr_idx, 0,
        "N0 (physical 90 %) should be the drain source"
    );
    assert_eq!(
        outcome.decision.drain_target_idx, 1,
        "N1 (physical 5 %) should be the drain target"
    );
    // Empty catalog → drain_node has zero objects to migrate. That
    // still counts as a fired-and-completed round.
    assert_eq!(outcome.objects_migrated, 0);
    assert_eq!(outcome.objects_failed, 0);

    // admin_kills[drain_idx] must be flipped back to false after
    // the tick — the auto-rebalancer is bounded, not a full
    // decommission. A leaked kill flag would silently remove the
    // node from all future PUTs.
    let kills_arc = gw.admin_kills_handle();
    let kills = kills_arc.lock().await;
    assert!(!kills[0], "admin_kill on drain node must be reset");
}

#[tokio::test]
async fn rebalance_tick_returns_none_when_no_skew() {
    // Both nodes below trigger — no round.
    let gw = build_gateway_with_addrs(vec!["node0".into(), "node1".into()]);
    let map = empty_map();
    inject_capacity(&map, "node0", 40, 100, 60).await;
    inject_capacity(&map, "node1", 50, 100, 50).await;

    let catalog: Arc<RwLock<Directory>> = Arc::new(RwLock::new(Directory::new()));
    let outcome = rebalance_tick(&gw, &map, &catalog, 85.0, 60.0, 4).await;
    assert!(
        outcome.is_none(),
        "no node above 85 % → tick must skip, got: {outcome:?}"
    );
    // admin_kills untouched — nothing fired.
    let kills_arc = gw.admin_kills_handle();
    let kills = kills_arc.lock().await;
    assert!(!kills[0]);
    assert!(!kills[1]);
}

#[tokio::test]
async fn rebalance_tick_returns_none_when_no_cold_target() {
    // Fullest above trigger BUT the "coldest" is still above
    // cold ceiling → no useful target → no round.
    let gw = build_gateway_with_addrs(vec!["node0".into(), "node1".into()]);
    let map = empty_map();
    inject_capacity(&map, "node0", 10, 100, 90).await; // physical 90
    inject_capacity(&map, "node1", 30, 100, 65).await; // physical 70

    let catalog: Arc<RwLock<Directory>> = Arc::new(RwLock::new(Directory::new()));
    let outcome = rebalance_tick(&gw, &map, &catalog, 85.0, 60.0, 4).await;
    assert!(
        outcome.is_none(),
        "coldest node also above ceiling → skip: {outcome:?}"
    );
}

#[tokio::test]
async fn rebalance_tick_ignores_unknown_capacity_entries() {
    // One node injected as known-empty, one as unknown ((0,0,0)
    // sentinel). Skew calc must ignore the unknown entry entirely.
    // Since there's now only one KNOWN node, `decide_rebalance`
    // returns None (needs ≥ 2 known nodes).
    let gw = build_gateway_with_addrs(vec!["node0".into(), "node1".into()]);
    let map = empty_map();
    inject_capacity(&map, "node0", 10, 100, 90).await;
    inject_capacity(&map, "node1", 0, 0, 0).await; // unknown sentinel

    let catalog: Arc<RwLock<Directory>> = Arc::new(RwLock::new(Directory::new()));
    let outcome = rebalance_tick(&gw, &map, &catalog, 85.0, 60.0, 4).await;
    assert!(
        outcome.is_none(),
        "single known node → decision needs ≥2 known → no round"
    );
}
