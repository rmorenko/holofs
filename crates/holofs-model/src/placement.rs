//! Placing shards across nodes.
//!
//! - [`Placement::RoundRobin`] — global round-robin: predictably uniform, but
//!   adding or removing a node reshuffles almost everything.
//! - [`Placement::Rendezvous`] — HRW (Highest Random Weight): each key is
//!   independently hashed against the node list and the maximum wins.
//!   Removing one node moves only that node's shards.
//! - [`Placement::RendezvousZoneAware`] — HRW + zone anti-affinity: for a
//!   single (channel, layer) each zone receives at most `ceil(n / z)` shards.
//!   This dampens the "whole rack failed → whole layer lost" risk — only the
//!   fraction `1/z` is lost, which survives the `K` threshold under enough
//!   redundancy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    RoundRobin,
    Rendezvous,
    RendezvousZoneAware,
}

/// Shard key — uniquely identifies a position inside an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardKey {
    pub object_id: u64,
    pub channel: u8,
    pub layer: u8,
    pub shard_idx: u32,
}

/// Returned by placement functions when there are zero live nodes
/// to choose from. Previously these functions asserted internally,
/// which crashed the gateway whenever the cluster was transiently
/// fully down (boot races, manual shutdowns). Now callers must
/// handle this explicitly — typically by surfacing a 503 / cluster-
/// degraded error to the user instead of panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoLiveNodes;

impl std::fmt::Display for NoLiveNodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live node set is empty")
    }
}

impl std::error::Error for NoLiveNodes {}

/// Pick a node from `live_nodes` (indices into the cluster node list).
/// Returns [`NoLiveNodes`] when there are no candidates — the caller
/// decides whether to retry, fail the request, or fall back.
///
/// For [`Placement::RendezvousZoneAware`] this function is equivalent to
/// Rendezvous — the zone constraint is enforced at the whole-layer level via
/// [`place_layer_zone_aware`]. The proper entry point for the zone-aware
/// version is `Manifest::place_shard`.
pub fn place(
    scheme: Placement,
    key: ShardKey,
    total_nodes: usize,
    live_nodes: &[usize],
) -> Result<usize, NoLiveNodes> {
    if live_nodes.is_empty() {
        return Err(NoLiveNodes);
    }
    Ok(match scheme {
        Placement::RoundRobin => {
            // Deterministic shift by (obj, c, l), then step = shard_idx.
            // On the full set → fair round-robin within the layer.
            let seed =
                mix64(key.object_id ^ ((key.channel as u64) << 16) ^ ((key.layer as u64) << 8));
            let pos = (seed.wrapping_add(key.shard_idx as u64)) as usize % total_nodes;
            // Step forward to the nearest live node (wrap around).
            for off in 0..total_nodes {
                let cand = (pos + off) % total_nodes;
                if live_nodes.binary_search(&cand).is_ok() {
                    return Ok(cand);
                }
            }
            unreachable!("live_nodes is non-empty but none matched")
        }
        Placement::Rendezvous | Placement::RendezvousZoneAware => {
            let mut best: (u64, usize) = (0, live_nodes[0]);
            for &node in live_nodes {
                let h = rendezvous_hash(key, node);
                if h >= best.0 {
                    best = (h, node);
                }
            }
            best.1
        }
    })
}

/// Zone-aware layout for an entire layer.
///
/// Algorithm: for each `shard_idx` sort live nodes by HRW score, take the
/// first whose zone has not yet exceeded the quota `ceil(n_shards / num_live_zones)`.
/// If every zone has hit its quota (which happens when n_shards > total quota
/// of live zones — an edge case under a heavily reduced live set), placement
/// is allowed on any zone — better to land somewhere than nowhere.
///
/// Deterministic in `(object_id, channel, layer)`, `live_nodes` and `zones`.
pub fn place_layer_zone_aware(
    object_id: u64,
    channel: u8,
    layer: u8,
    n_shards: u32,
    total_nodes: usize,
    live_nodes: &[usize],
    zones: &[u8],
) -> Result<Vec<usize>, NoLiveNodes> {
    if live_nodes.is_empty() {
        return Err(NoLiveNodes);
    }
    assert_eq!(zones.len(), total_nodes, "zones and total_nodes disagree");

    // How many distinct zones are represented among live nodes?
    let mut live_zone_ids: Vec<u8> = live_nodes.iter().map(|&n| zones[n]).collect();
    live_zone_ids.sort();
    live_zone_ids.dedup();
    let num_live_zones = live_zone_ids.len().max(1);
    // ceil(n / z): aim for uniform spread. An extra +1 breaks nothing — the
    // goal is anti-affinity, not perfect counts.
    let quota: u32 = (n_shards + num_live_zones as u32 - 1) / num_live_zones as u32;

    let mut zone_used: std::collections::HashMap<u8, u32> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(n_shards as usize);

    for idx in 0..n_shards {
        let key = ShardKey {
            object_id,
            channel,
            layer,
            shard_idx: idx,
        };
        // All live nodes with their scores, sorted descending.
        let mut scored: Vec<(u64, usize)> = live_nodes
            .iter()
            .map(|&n| (rendezvous_hash(key, n), n))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        // Pick the first whose zone is still within quota.
        let mut chosen: Option<usize> = None;
        for (_, node) in &scored {
            let zone = zones[*node];
            if *zone_used.get(&zone).unwrap_or(&0) < quota {
                chosen = Some(*node);
                break;
            }
        }
        let node = chosen.unwrap_or_else(|| scored[0].1);
        *zone_used.entry(zones[node]).or_insert(0) += 1;
        out.push(node);
    }
    Ok(out)
}

/// Stage 15.1 replicated encoding: pick the top-`replication` nodes
/// for a given block by HRW score, in descending order (highest
/// score first). Zone-aware placement is deliberately NOT applied
/// here — replication already spreads a block across R nodes and
/// forcing zone diversity on top of that would double the fault
/// budget per block. Callers that want zone-aware replicas can
/// layer that on top by picking the first R distinct-zone entries
/// from `sort_nodes_by_hrw` output.
///
/// Returns [`NoLiveNodes`] when `live_nodes` is empty. Truncates
/// silently when `live_nodes.len() < replication` — the placement is
/// best-effort, matching how RLNC's `place` handles under-provisioned
/// clusters.
pub fn place_replicas(
    key: ShardKey,
    replication: u8,
    live_nodes: &[usize],
) -> Result<Vec<usize>, NoLiveNodes> {
    if live_nodes.is_empty() {
        return Err(NoLiveNodes);
    }
    let mut scored: Vec<(u64, usize)> = live_nodes
        .iter()
        .map(|&n| (rendezvous_hash(key, n), n))
        .collect();
    // Descending HRW — same convention as `place`.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(scored
        .into_iter()
        .take(replication as usize)
        .map(|(_, n)| n)
        .collect())
}

fn rendezvous_hash(key: ShardKey, node: usize) -> u64 {
    let mut h = mix64(key.object_id);
    h ^= mix64((node as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    h ^= mix64(((key.shard_idx as u64) << 16) | ((key.channel as u64) << 8) | (key.layer as u64));
    mix64(h)
}

/// SplitMix64 — a cheap avalanche function with no dependencies.
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(idx: u32) -> ShardKey {
        ShardKey {
            object_id: 0xABCD,
            channel: 1,
            layer: 2,
            shard_idx: idx,
        }
    }

    fn live_range(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    #[test]
    fn rendezvous_is_deterministic() {
        let live = live_range(8);
        for i in 0..1024 {
            let a = place(Placement::Rendezvous, key(i), 8, &live).unwrap();
            let b = place(Placement::Rendezvous, key(i), 8, &live).unwrap();
            assert_eq!(a, b);
        }
    }

    #[test]
    fn rendezvous_distributes_roughly_evenly() {
        let nodes = 16usize;
        let live = live_range(nodes);
        let mut hits = vec![0usize; nodes];
        for i in 0..10_000 {
            hits[place(Placement::Rendezvous, key(i), nodes, &live).unwrap()] += 1;
        }
        // Expected ~625, allow ±50% (no chi-squared — this is a smoke test).
        for (i, &h) in hits.iter().enumerate() {
            assert!(
                h > 300 && h < 1000,
                "node {i}: {h} hits is out of range"
            );
        }
    }

    #[test]
    fn rendezvous_minimal_disturbance_on_node_removal() {
        // Key HRW property: removing one node moves only that node's shards.
        let nodes = 16usize;
        let full = live_range(nodes);
        let mut without = full.clone();
        without.remove(7);

        let mut moved_from_others = 0;
        let mut moved_from_dead = 0;
        let trials = 5_000;
        for i in 0..trials {
            let a = place(Placement::Rendezvous, key(i), nodes, &full).unwrap();
            let b = place(Placement::Rendezvous, key(i), nodes, &without).unwrap();
            if a == 7 {
                moved_from_dead += 1;
                assert_ne!(b, 7);
            } else if a != b {
                moved_from_others += 1;
            }
        }
        // Shards that did not live on the removed node must have stayed put.
        assert_eq!(
            moved_from_others, 0,
            "HRW incomplete: {moved_from_others} shards moved although their node is alive"
        );
        // Every shard on the dead node must have moved.
        assert!(moved_from_dead > 0);
    }

    #[test]
    fn round_robin_skips_dead_nodes() {
        let live = vec![0, 2, 4, 6]; // odd indices are dead
        for i in 0..200 {
            let n = place(Placement::RoundRobin, key(i), 8, &live).unwrap();
            assert!(live.contains(&n), "node {n} is dead");
        }
    }

    #[test]
    fn round_robin_covers_all_live_nodes() {
        let live = live_range(8);
        let mut seen = [false; 8];
        for i in 0..256 {
            seen[place(Placement::RoundRobin, key(i), 8, &live).unwrap()] = true;
        }
        assert!(seen.iter().all(|&b| b), "round-robin did not cover every node");
    }

    // Helpers: "4 zones × 4 nodes" topology.
    fn zones_4x4() -> Vec<u8> {
        (0..16u8).map(|i| i / 4).collect()
    }

    #[test]
    fn zone_aware_layout_respects_quota() {
        let zones = zones_4x4();
        let live: Vec<usize> = (0..16).collect();
        let layout = place_layer_zone_aware(0xABCD, 0, 0, 16, 16, &live, &zones).unwrap();
        // 16 shards, 4 zones → quota 4 per zone. Each zone must receive exactly 4.
        let mut per_zone = [0u32; 4];
        for &n in &layout {
            per_zone[zones[n] as usize] += 1;
        }
        for (i, &c) in per_zone.iter().enumerate() {
            assert_eq!(c, 4, "zone {i} got {c} shards, expected 4");
        }
    }

    #[test]
    fn zone_aware_layout_is_deterministic() {
        let zones = zones_4x4();
        let live: Vec<usize> = (0..16).collect();
        let a = place_layer_zone_aware(0xC0FFEE, 1, 2, 12, 16, &live, &zones).unwrap();
        let b = place_layer_zone_aware(0xC0FFEE, 1, 2, 12, 16, &live, &zones).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn zone_aware_drops_failed_zone_only_partially() {
        // If zone 0 is fully dead, shards must land in the 3 remaining zones
        // and the layout must contain zero shards in zone 0.
        let zones = zones_4x4();
        let live: Vec<usize> = (4..16).collect(); // zone 0 (nodes 0..3) is dead
        let layout = place_layer_zone_aware(0xDEAD, 0, 0, 12, 16, &live, &zones).unwrap();
        let mut per_zone = [0u32; 4];
        for &n in &layout {
            per_zone[zones[n] as usize] += 1;
        }
        assert_eq!(per_zone[0], 0, "dead zone 0 must hold no shards");
        // 12 shards across 3 live zones → exactly 4 per zone.
        for z in 1..4 {
            assert_eq!(per_zone[z], 4);
        }
    }

    #[test]
    fn zone_aware_diverges_from_plain_rendezvous_under_cluster_skew() {
        // On a full 4×4 cluster zone-aware produces strictly uniform zone
        // counts; plain Rendezvous does not (it clusters by nature).
        let zones = zones_4x4();
        let live: Vec<usize> = (0..16).collect();
        let za = place_layer_zone_aware(0xBEEF, 0, 0, 16, 16, &live, &zones).unwrap();
        let mut plain = Vec::with_capacity(16);
        for idx in 0..16 {
            plain.push(
                place(
                    Placement::Rendezvous,
                    ShardKey {
                        object_id: 0xBEEF,
                        channel: 0,
                        layer: 0,
                        shard_idx: idx,
                    },
                    16,
                    &live,
                )
                .unwrap(),
            );
        }
        let count_per_zone = |layout: &[usize]| -> Vec<u32> {
            let mut v = vec![0u32; 4];
            for &n in layout {
                v[zones[n] as usize] += 1;
            }
            v
        };
        let za_counts = count_per_zone(&za);
        let plain_counts = count_per_zone(&plain);
        // ZA — exactly 4 each. Plain almost surely has at least one zone != 4.
        assert!(za_counts.iter().all(|&c| c == 4));
        assert!(plain_counts.iter().any(|&c| c != 4));
    }

    #[test]
    fn place_returns_error_on_empty_live() {
        let empty: Vec<usize> = Vec::new();
        let r = place(Placement::Rendezvous, key(0), 16, &empty);
        assert!(matches!(r, Err(NoLiveNodes)));
        let r = place(Placement::RoundRobin, key(0), 16, &empty);
        assert!(matches!(r, Err(NoLiveNodes)));
    }

    #[test]
    fn place_layer_zone_aware_returns_error_on_empty_live() {
        let zones = zones_4x4();
        let empty: Vec<usize> = Vec::new();
        let r = place_layer_zone_aware(0x1234, 0, 0, 8, 16, &empty, &zones);
        assert!(matches!(r, Err(NoLiveNodes)));
    }

    #[test]
    fn place_replicas_returns_r_distinct_nodes() {
        let live: Vec<usize> = (0..8).collect();
        let out = place_replicas(key(42), 3, &live).unwrap();
        assert_eq!(out.len(), 3);
        // All distinct.
        let mut sorted = out.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn place_replicas_is_deterministic() {
        let live: Vec<usize> = (0..8).collect();
        for i in 0..64 {
            let a = place_replicas(key(i), 3, &live).unwrap();
            let b = place_replicas(key(i), 3, &live).unwrap();
            assert_eq!(a, b);
        }
    }

    #[test]
    fn place_replicas_truncates_when_live_smaller_than_r() {
        let live: Vec<usize> = vec![1, 5];
        let out = place_replicas(key(7), 3, &live).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|n| live.contains(n)));
    }

    #[test]
    fn place_replicas_first_matches_place_rendezvous() {
        // Top-1 replica set must equal the single Rendezvous pick.
        let live: Vec<usize> = (0..8).collect();
        for i in 0..64 {
            let solo = place(Placement::Rendezvous, key(i), 8, &live).unwrap();
            let rep = place_replicas(key(i), 1, &live).unwrap();
            assert_eq!(vec![solo], rep);
        }
    }

    #[test]
    fn place_replicas_returns_error_on_empty_live() {
        let empty: Vec<usize> = Vec::new();
        assert!(matches!(
            place_replicas(key(0), 3, &empty),
            Err(NoLiveNodes)
        ));
    }
}
