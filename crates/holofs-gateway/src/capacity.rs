//! P1.4b — gateway-side capacity tracking + auto-rebalance policy.
//!
//! # What lives here
//!
//! - [`CapacityMap`]: per-node snapshot of the last successful
//!   `Request::Capacity` reply, keyed by the node's address. Wrapped
//!   in `Arc<Mutex<...>>` and hung off `Gateway.capacity_map`.
//! - [`spawn_capacity_poller`]: background task that polls every node
//!   in `cluster.node_addrs` on a fixed interval (default 60 s) and
//!   updates the map.
//! - [`spawn_auto_rebalancer`]: background task that reads the map on
//!   its own (slower) cadence, decides whether cluster skew warrants a
//!   proactive drain-partial, and — if yes — invokes the same
//!   `drain_node`-style migration path used by the manual admin CLI.
//!   Rate-limited so it can't loop-storm the cluster.
//!
//! # Why polling (not push)
//!
//! Nodes are stateless w.r.t. the gateway — a push would need extra
//! wire semantics (connection lifecycle, retries, ack) that don't earn
//! their weight for a signal that changes on the order of minutes. A
//! 60 s poll adds one RPC per node per minute (~microseconds of
//! server-side work each) — negligible next to the PUT hot path.
//!
//! # Freshness contract
//!
//! Entries have an `updated_at: Instant`. A missing entry (node never
//! answered) is treated identically to `total_bytes = 0` (unknown
//! capacity) — the auto-rebalancer skips it in skew calculations. A
//! *stale* entry (poll failed for the last N ticks) is retained but
//! its `updated_at` doesn't advance; downstream can inspect the
//! staleness gap and downgrade to unknown after a threshold.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use holofs_client::{node_capacity, NodeCapacity};
use holofs_model::fs::Directory;

use crate::http_gateway::ClusterInfo;

/// Default poll interval — every 60 s. Overridable at boot via
/// `HOLOFS_CAPACITY_POLL_INTERVAL_SECS`. The value here is a floor
/// against operator-typo (`0` → busy loop); the reader clamps to at
/// least 5 s.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// Default auto-rebalance decision interval — every 5 min. Slower
/// than the poll on purpose: the rebalancer wants to see steady-state
/// numbers, not react to a single spike. Overridable at boot via
/// `HOLOFS_REBALANCE_INTERVAL_SECS`. `0` disables the rebalancer.
pub const DEFAULT_REBALANCE_INTERVAL_SECS: u64 = 300;

/// Used-pct threshold that triggers a rebalance round. Env override:
/// `HOLOFS_REBALANCE_TRIGGER_PCT`. 85 % is deliberately conservative —
/// well below the 95-100 % zone where writes actually start refusing,
/// but high enough that a healthy 3-node cluster with routine growth
/// doesn't churn on every tick.
pub const DEFAULT_REBALANCE_TRIGGER_PCT: f64 = 85.0;

/// Coldest-node ceiling: if the *emptiest* node is already above this,
/// there's no useful drain target left, so the rebalancer skips this
/// round rather than shuffling storage between two nearly-full nodes.
/// Env override: `HOLOFS_REBALANCE_COLD_CEILING_PCT`.
pub const DEFAULT_REBALANCE_COLD_CEILING_PCT: f64 = 60.0;

/// One node's entry in the [`CapacityMap`]. `updated_at` lets the
/// auto-rebalancer downgrade stale readings to unknown instead of
/// making a bad call on last-known-good numbers.
#[derive(Debug, Clone, Copy)]
pub struct CapacityEntry {
    /// Last successful [`NodeCapacity`] reading. `is_known()` returns
    /// `false` when the node reported `(0, 0, 0)`.
    pub capacity: NodeCapacity,
    /// Wall-clock at the time of the last successful reading.
    pub updated_at: Instant,
}

/// Shared map keyed by node address (as stored in
/// `ClusterInfo.node_addrs`). `Mutex` not `RwLock` because the writer
/// (poller) runs every 60 s and the readers (HTTP handlers,
/// rebalancer) are also low-frequency — the lock is never contended.
pub type CapacityMap = Arc<Mutex<HashMap<String, CapacityEntry>>>;

/// Fresh, empty capacity map. Called once at gateway construction.
#[must_use]
pub fn empty_map() -> CapacityMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Snapshot the map's current contents into an owned `HashMap`. Used
/// by HTTP handlers that want to render the values without holding
/// the mutex across `await` points.
pub async fn snapshot(map: &CapacityMap) -> HashMap<String, CapacityEntry> {
    map.lock().await.clone()
}

/// Poll every node in `addrs` once (concurrently) and refresh
/// `map`. A poll RPC failure preserves the previous entry — useful
/// for a briefly-partitioned node that's expected to come back.
///
/// Returns the count of nodes that answered on this round; the
/// auto-rebalancer uses that to skip decisions when the cluster is
/// mostly unreachable.
pub async fn poll_once(addrs: &[String], map: &CapacityMap) -> usize {
    let mut successes = 0usize;
    // Run per-node polls concurrently — one slow node shouldn't
    // stretch the tick beyond the interval.
    let futs: Vec<_> = addrs
        .iter()
        .cloned()
        .map(|addr| async move {
            let r = node_capacity(&addr).await;
            (addr, r)
        })
        .collect();
    let results = futures_util::future::join_all(futs).await;
    let now = Instant::now();
    let mut g = map.lock().await;
    for (addr, res) in results {
        match res {
            Ok(capacity) => {
                g.insert(
                    addr,
                    CapacityEntry {
                        capacity,
                        updated_at: now,
                    },
                );
                successes += 1;
            }
            Err(_) => {
                // Keep the previous entry (if any) — a single-tick
                // hiccup shouldn't erase the last-known reading.
            }
        }
    }
    successes
}

/// Spawn a background task that refreshes `map` every
/// `interval_secs` seconds by polling every address in `cluster.
/// node_addrs`. Returns the join handle so the caller (bootstrap)
/// can hold it for lifetime purposes. The task is cancellation-safe:
/// dropping the handle aborts it.
pub fn spawn_capacity_poller(
    cluster: Arc<ClusterInfo>,
    map: CapacityMap,
    interval_secs: u64,
) -> JoinHandle<()> {
    let interval = Duration::from_secs(interval_secs.max(5));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // `MissedTickBehavior::Delay` — if a poll round takes longer
        // than one interval (unlikely but possible under a wedged
        // cluster), skip the missed ticks rather than firing back-to-
        // back and pinning the executor.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let n_answered = poll_once(&cluster.node_addrs, &map).await;
            // Skew observability: log a WARN when the used-pct spread
            // between the fullest and emptiest known node crosses
            // 3× — even the observability-only mode leaves this
            // breadcrumb for operators looking at logs.
            if n_answered >= 2 {
                let snap: Vec<_> = {
                    let g = map.lock().await;
                    g.values().copied().collect()
                };
                if let Some((min, max)) = min_max_used_pct(&snap) {
                    if max > 0.0 && min >= 0.0 && (max / min.max(1.0)) >= 3.0 {
                        tracing::warn!(
                            min_used_pct = min,
                            max_used_pct = max,
                            n_answered,
                            "cluster capacity skew: fullest node is >= 3x the emptiest — \
                             consider `holofs-admin capacity` + drain-node"
                        );
                    }
                }
            }
        }
    })
}

/// Return `(min_used_pct, max_used_pct)` across `entries` that report
/// known capacity. `None` when fewer than one entry is known. Small,
/// standalone helper so the auto-rebalancer + the poller-side skew
/// warning can share the same math.
#[must_use]
pub fn min_max_used_pct(entries: &[CapacityEntry]) -> Option<(f64, f64)> {
    let mut it = entries
        .iter()
        .filter(|e| e.capacity.is_known())
        .map(|e| e.capacity.used_pct());
    let first = it.next()?;
    let (mut min, mut max) = (first, first);
    for v in it {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    Some((min, max))
}

/// Auto-rebalance policy: given the current capacity snapshot and the
/// operator-configured trigger + cold-ceiling thresholds, decide
/// whether *this* round should fire and — if yes — which node to
/// drain (the fullest) and which to prefer as drain target (the
/// emptiest). `None` means "no action this round".
///
/// Extracted as a pure function so the decision is directly unit-
/// testable without spinning up nodes, HTTP, or an executor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RebalanceDecision {
    /// Address of the node that's above `trigger_pct` and holds the
    /// most `live_bytes` (proxy for "worst offender").
    pub drain_addr_idx: usize,
    /// Address of the coldest known node with `used_pct < cold_ceiling_pct`.
    pub drain_target_idx: usize,
    /// The full-node's used percentage at decision time (for logs).
    pub full_used_pct: f64,
    /// The cold-node's used percentage at decision time (for logs).
    pub cold_used_pct: f64,
}

/// Same-return-shape wrapper around the map-lookup + policy check.
/// `addrs` is the cluster's authoritative node-address vec; the
/// returned indices are indices *into that vec*, so callers can feed
/// them straight into `admin_kills` / `drain_node` without an extra
/// address→index lookup.
#[must_use]
pub fn decide_rebalance(
    addrs: &[String],
    entries: &HashMap<String, CapacityEntry>,
    trigger_pct: f64,
    cold_ceiling_pct: f64,
) -> Option<RebalanceDecision> {
    // Collect (idx, entry) pairs only for nodes we know about.
    let known: Vec<(usize, &CapacityEntry)> = addrs
        .iter()
        .enumerate()
        .filter_map(|(i, a)| entries.get(a).map(|e| (i, e)))
        .filter(|(_, e)| e.capacity.is_known())
        .collect();
    if known.len() < 2 {
        return None;
    }

    // Fullest: max used_pct, tiebreak on live_bytes so we prefer
    // dumping the one with the most physical footprint.
    let (drain_idx, drain_entry) = known
        .iter()
        .copied()
        .max_by(|(_, a), (_, b)| {
            a.capacity
                .used_pct()
                .partial_cmp(&b.capacity.used_pct())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.capacity.live_bytes.cmp(&b.capacity.live_bytes))
        })
        .unwrap();
    let full_used_pct = drain_entry.capacity.used_pct();
    if full_used_pct < trigger_pct {
        return None;
    }

    // Coldest: min used_pct AND under the cold ceiling — otherwise
    // there's no useful target left in this cluster.
    let (target_idx, target_entry) = known
        .iter()
        .copied()
        .filter(|(idx, _)| *idx != drain_idx)
        .min_by(|(_, a), (_, b)| {
            a.capacity
                .used_pct()
                .partial_cmp(&b.capacity.used_pct())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let cold_used_pct = target_entry.capacity.used_pct();
    if cold_used_pct >= cold_ceiling_pct {
        return None;
    }
    Some(RebalanceDecision {
        drain_addr_idx: drain_idx,
        drain_target_idx: target_idx,
        full_used_pct,
        cold_used_pct,
    })
}

/// Spawn the auto-rebalancer background task. Ticks every
/// `interval_secs`; on each tick reads the capacity map, feeds it
/// into [`decide_rebalance`], and — on a positive decision — invokes
/// a *bounded* partial drain (not a full drain) via
/// [`holofs_cluster::rebalance::drain_node`]. Bounded means: flip
/// `admin_kills[drain_idx] = true`, run drain, flip it back off.
/// Because `drain_node` is idempotent, a subsequent tick that still
/// sees skew will drain more; if the cluster is now balanced,
/// `decide_rebalance` returns `None` and the round is skipped.
///
/// `trigger_pct = 0.0` OR `interval_secs = 0` disables the daemon
/// entirely — used by tests that don't want a background rebalancer.
pub fn spawn_auto_rebalancer(
    gateway: Arc<crate::http_gateway::Gateway>,
    map: CapacityMap,
    catalog: Arc<RwLock<Directory>>,
    interval_secs: u64,
    trigger_pct: f64,
    cold_ceiling_pct: f64,
    repair_d: usize,
) -> Option<JoinHandle<()>> {
    if interval_secs == 0 || trigger_pct <= 0.0 {
        return None;
    }
    let interval = Duration::from_secs(interval_secs.max(30));
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't shove a
        // rebalance the moment the gateway boots (before the poller
        // has even had a chance to fill the map once).
        tick.tick().await;
        loop {
            tick.tick().await;

            // Snapshot cluster + capacity under the map lock, then
            // release before the (potentially long) drain.
            let addrs = gateway.cluster.node_addrs.clone();
            let entries = snapshot(&map).await;
            let Some(decision) = decide_rebalance(&addrs, &entries, trigger_pct, cold_ceiling_pct)
            else {
                continue;
            };

            let drain_idx = decision.drain_addr_idx;
            tracing::info!(
                drain_idx,
                drain_addr = %addrs.get(drain_idx).cloned().unwrap_or_default(),
                target_idx = decision.drain_target_idx,
                target_addr = %addrs.get(decision.drain_target_idx).cloned().unwrap_or_default(),
                full_used_pct = decision.full_used_pct,
                cold_used_pct = decision.cold_used_pct,
                "auto-rebalance triggered: draining fullest node"
            );

            // Flip admin_kill on the drain node, run drain, flip
            // back. This is intentionally NOT the same as the manual
            // `drain-node` command: we don't want to permanently
            // remove the node — we just want to migrate a chunk of
            // its shards to the emptier node under a temporary
            // "logically dead" flag. When the flag flips off,
            // subsequent PUTs can land on it again — but its used_pct
            // has dropped, and the fresh writes go to the emptier
            // node (HRW is uniform, but the drained fraction stays
            // gone).
            {
                let mut kills = gateway.admin_kills.lock().await;
                if drain_idx < kills.len() {
                    kills[drain_idx] = true;
                }
            }

            let live_before: holofs_client::LiveNodes = {
                let kills = gateway.admin_kills.lock().await;
                (0..addrs.len())
                    .filter(|i| !*kills.get(*i).unwrap_or(&true))
                    // include drain_idx explicitly in the before-list
                    // so drain_node_from_manifest sees it there.
                    .chain(std::iter::once(drain_idx))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };

            let gf = Arc::clone(&gateway.gf);
            let mut rng = holofs_core::rng::Rng::new(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0))
                .wrapping_add(drain_idx as u64),
            );

            let reports = holofs_cluster::rebalance::drain_node(
                &gf,
                &mut rng,
                Arc::clone(&catalog),
                drain_idx,
                live_before,
                repair_d,
            )
            .await;

            // Flip the kill flag back off — the node is not decommissioned,
            // just rebalanced.
            {
                let mut kills = gateway.admin_kills.lock().await;
                if drain_idx < kills.len() {
                    kills[drain_idx] = false;
                }
            }

            let ok = reports.iter().filter(|r| r.result.is_ok()).count();
            let failed = reports.len() - ok;
            tracing::info!(
                ok,
                failed,
                total = reports.len(),
                drain_idx,
                "auto-rebalance round complete"
            );
        }
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(free: u64, total: u64, live: u64) -> CapacityEntry {
        CapacityEntry {
            capacity: NodeCapacity {
                free_bytes: free,
                total_bytes: total,
                live_bytes: live,
            },
            updated_at: Instant::now(),
        }
    }

    fn unknown() -> CapacityEntry {
        CapacityEntry {
            capacity: NodeCapacity {
                free_bytes: 0,
                total_bytes: 0,
                live_bytes: 0,
            },
            updated_at: Instant::now(),
        }
    }

    #[test]
    fn min_max_used_pct_ignores_unknown_entries() {
        let entries = vec![known(0, 100, 90), unknown(), known(0, 100, 30)];
        let (min, max) = min_max_used_pct(&entries).unwrap();
        assert!((min - 30.0).abs() < 0.01);
        assert!((max - 90.0).abs() < 0.01);
    }

    #[test]
    fn min_max_used_pct_returns_none_when_all_unknown() {
        let entries = vec![unknown(), unknown()];
        assert!(min_max_used_pct(&entries).is_none());
    }

    #[test]
    fn decide_rebalance_fires_when_full_and_cold_exist() {
        // 3-node cluster: A=90 %, B=50 %, C=20 %. Trigger=85, cold-
        // ceiling=60. Expect drain=A (idx 0), target=C (idx 2).
        let addrs: Vec<String> = vec!["A".into(), "B".into(), "C".into()];
        let mut entries = HashMap::new();
        entries.insert("A".into(), known(10, 100, 90));
        entries.insert("B".into(), known(50, 100, 50));
        entries.insert("C".into(), known(80, 100, 20));
        let d = decide_rebalance(&addrs, &entries, 85.0, 60.0).unwrap();
        assert_eq!(d.drain_addr_idx, 0);
        assert_eq!(d.drain_target_idx, 2);
    }

    #[test]
    fn decide_rebalance_skips_when_no_node_over_trigger() {
        let addrs: Vec<String> = vec!["A".into(), "B".into()];
        let mut entries = HashMap::new();
        entries.insert("A".into(), known(30, 100, 70));
        entries.insert("B".into(), known(50, 100, 50));
        assert!(decide_rebalance(&addrs, &entries, 85.0, 60.0).is_none());
    }

    #[test]
    fn decide_rebalance_skips_when_all_cold_nodes_too_full() {
        // A=90 % triggers, but B=65 % is already over cold_ceiling=60,
        // so there's no useful target — decision is None.
        let addrs: Vec<String> = vec!["A".into(), "B".into()];
        let mut entries = HashMap::new();
        entries.insert("A".into(), known(10, 100, 90));
        entries.insert("B".into(), known(35, 100, 65));
        assert!(decide_rebalance(&addrs, &entries, 85.0, 60.0).is_none());
    }

    #[test]
    fn decide_rebalance_needs_at_least_two_known_nodes() {
        let addrs: Vec<String> = vec!["A".into(), "B".into()];
        let mut entries = HashMap::new();
        entries.insert("A".into(), known(10, 100, 90));
        entries.insert("B".into(), unknown());
        assert!(decide_rebalance(&addrs, &entries, 85.0, 60.0).is_none());
    }
}
