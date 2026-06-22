//! Stage 5: object health dashboard.
//!
//! Per object we compute:
//! - **Redundancy margin** per (channel, layer): how many shards are actually
//!   alive vs `K`. The query goes over the network via
//!   [`holofs_client::gather_layer`]: a hypothetical "by plan" may diverge
//!   from reality after repairs and node deaths; we care about reality.
//! - **Current resolution** of the object: the maximum layer `L` such that
//!   all `0..=L` decode across all channels. Holographic property: below the
//!   threshold the object becomes "fuzzier", not "absent".
//! - **Random loss simulation X%** (Monte Carlo): purely computational on top
//!   of the HRW layout of the current live set. Accounts for real shard
//!   clustering (HRW yields an uneven shards-per-node count).
//!
//! Pure logic with no network calls except [`collect_layer_stats`].
//! The simulator is deterministic — the seed comes from the object CID, so a
//! repeated call on the same inputs returns identical numbers.

use std::collections::HashSet;

use holofs_client::{gather_layer, ClientError, LiveNodes};
use holofs_core::merkle::{shard_hash, Hash};
use holofs_core::rng::Rng;
use holofs_model::manifest::Manifest;

/// Default random-loss scenarios (percentage of nodes killed at random).
pub const DEFAULT_KILL_PCTS: [u8; 4] = [10, 25, 50, 75];

/// Number of Monte-Carlo trials per kill_pct.
pub const DEFAULT_N_TRIALS: u32 = 5_000;

/// Statistics for a single (channel, layer) of an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerStats {
    pub channel: u8,
    pub layer: u8,
    /// Number of shards planned for this layer in the manifest.
    pub n_shards: u32,
    /// Number of shards actually alive: answered GET and passed hash verification.
    pub n_alive: u32,
    /// Number of nodes actually hosting this (channel, layer) — HRW over live nodes.
    pub n_hosting_nodes: u32,
    /// Decoding threshold.
    pub k: u16,
    /// `n_alive - k`. Negative → the layer no longer decodes.
    pub margin: i32,
}

impl LayerStats {
    pub fn decodable(&self) -> bool {
        self.margin >= 0
    }
}

/// Random-loss scenario: distribution of "max surviving layer".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossScenario {
    pub kill_pct: u8,
    pub n_trials: u32,
    /// `dist[0]` — dead (no layer), `dist[1]` — L0 only, ..., `dist[nlayers]` — all layers.
    pub dist: Vec<u32>,
}

impl LossScenario {
    /// Normalised distribution as fractions of 1.
    pub fn pmf(&self) -> Vec<f32> {
        let n = self.n_trials.max(1) as f32;
        self.dist.iter().map(|&c| c as f32 / n).collect()
    }
}

/// Full per-object summary: name, metadata, per-layer stats, and loss simulations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectHealth {
    pub object_id: u64,
    pub name: String,
    pub data_cid: Hash,
    pub n_nodes: usize,
    pub n_live: usize,
    pub nlayers: u8,
    pub channels: u8,
    pub k: u16,
    pub layers: Vec<LayerStats>,
    /// Current resolution: max fully decodable layer. `None` — object is dead.
    pub current_resolution: Option<u8>,
    pub scenarios: Vec<LossScenario>,
    /// Deterministic "whole zone Z dies" scenarios. No Monte Carlo — zone loss
    /// is deterministic. Empty when all nodes live in the same zone
    /// (zone-aware is not active).
    pub zone_failures: Vec<ZoneFailure>,
}

/// "What remains if zone Z dies entirely". Deterministic scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneFailure {
    pub zone: u8,
    pub nodes_in_zone: u32,
    /// Maximum layer that still decodes. `None` — object is dead.
    pub resolution: Option<u8>,
}

/// Poll the cluster: count live shards and hosting nodes per (channel, layer).
pub async fn collect_layer_stats(
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<Vec<LayerStats>, ClientError> {
    let live_sorted = sorted(live);
    let mut out = Vec::with_capacity((manifest.channels as usize) * (manifest.nlayers as usize));
    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            let raw = gather_layer(manifest, live, c, l).await?;
            let expected: HashSet<Hash> = manifest.shard_hashes[c as usize][l as usize]
                .iter()
                .copied()
                .collect();
            let n_alive = raw
                .iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .count() as u32;
            let mut hosting: HashSet<usize> = HashSet::new();
            for idx in 0..manifest.n_per_layer[l as usize] {
                hosting.insert(manifest.place_shard(c, l, idx, &live_sorted));
            }
            out.push(LayerStats {
                channel: c,
                layer: l,
                n_shards: manifest.n_per_layer[l as usize],
                n_alive,
                n_hosting_nodes: hosting.len() as u32,
                k: manifest.k,
                margin: n_alive as i32 - manifest.k as i32,
            });
        }
    }
    Ok(out)
}

/// Current resolution: max L such that every layer 0..=L decodes in every channel.
pub fn current_resolution(stats: &[LayerStats], channels: u8, nlayers: u8) -> Option<u8> {
    let mut max: Option<u8> = None;
    for l in 0..nlayers {
        let mut all_ok = true;
        for c in 0..channels {
            let ok = stats
                .iter()
                .find(|s| s.channel == c && s.layer == l)
                .map(|s| s.decodable())
                .unwrap_or(false);
            if !ok {
                all_ok = false;
                break;
            }
        }
        if all_ok {
            max = Some(l);
        } else {
            break;
        }
    }
    max
}

/// Monte Carlo: for each `kill_pct` runs `n_trials` random-death trials and
/// collects the "max surviving layer" distribution.
///
/// Layout is computed purely from the current `live` set via HRW — past
/// repairs are ignored; only "where shards would land if PUT were now". This
/// approximates the steady state after repairs and yields reproducible numbers.
pub fn simulate_loss(
    manifest: &Manifest,
    live: &LiveNodes,
    kill_pcts: &[u8],
    n_trials: u32,
    rng: &mut Rng,
) -> Vec<LossScenario> {
    let live_sorted = sorted(live);
    let layout = build_layout(manifest, &live_sorted);
    let k = manifest.k as u32;
    let mut out = Vec::with_capacity(kill_pcts.len());
    for &kp in kill_pcts {
        let n_kill = (live.len() * kp as usize / 100).min(live.len());
        let mut dist = vec![0u32; manifest.nlayers as usize + 1];
        let mut perm: Vec<usize> = live.clone();
        for _ in 0..n_trials {
            // Partial Fisher-Yates: shuffle only the prefix of length n_kill.
            for i in 0..n_kill {
                let j = i + (rng.next() as usize) % (perm.len() - i);
                perm.swap(i, j);
            }
            let dead: HashSet<usize> = perm[..n_kill].iter().copied().collect();
            let mut resolution: i32 = -1;
            for l in 0..manifest.nlayers as usize {
                let mut all_ok = true;
                for c in 0..manifest.channels as usize {
                    let alive = layout[c][l].iter().filter(|n| !dead.contains(n)).count() as u32;
                    if alive < k {
                        all_ok = false;
                        break;
                    }
                }
                if all_ok {
                    resolution = l as i32;
                } else {
                    break;
                }
            }
            dist[(resolution + 1) as usize] += 1;
        }
        out.push(LossScenario {
            kill_pct: kp,
            n_trials,
            dist,
        });
    }
    out
}

/// Full summary: poll + current-resolution computation + loss simulation.
pub async fn object_health(
    name: &str,
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<ObjectHealth, ClientError> {
    let layers = collect_layer_stats(manifest, live).await?;
    let current = current_resolution(&layers, manifest.channels, manifest.nlayers);
    let seed = u64::from_be_bytes(manifest.data_cid[0..8].try_into().unwrap());
    let mut rng = Rng::new(seed);
    let scenarios = simulate_loss(
        manifest,
        live,
        &DEFAULT_KILL_PCTS,
        DEFAULT_N_TRIALS,
        &mut rng,
    );
    let zone_failures = simulate_zone_failures(manifest, live);
    Ok(ObjectHealth {
        object_id: manifest.object_id,
        name: name.to_string(),
        data_cid: manifest.data_cid,
        n_nodes: manifest.nodes.len(),
        n_live: live.len(),
        nlayers: manifest.nlayers,
        channels: manifest.channels,
        k: manifest.k,
        layers,
        current_resolution: current,
        scenarios,
        zone_failures,
    })
}

/// For each distinct cluster zone: "what happens if that zone dies entirely".
/// Returns an empty list when the cluster has a single zone (zone-aware unused).
pub fn simulate_zone_failures(manifest: &Manifest, live: &LiveNodes) -> Vec<ZoneFailure> {
    if manifest.zones.is_empty() {
        return Vec::new();
    }
    let unique_zones: HashSet<u8> = manifest.zones.iter().copied().collect();
    if unique_zones.len() <= 1 {
        return Vec::new();
    }
    let live_sorted = sorted(live);
    let layout = build_layout(manifest, &live_sorted);
    let k = manifest.k as u32;
    let mut zones_sorted: Vec<u8> = unique_zones.into_iter().collect();
    zones_sorted.sort();
    let mut out = Vec::with_capacity(zones_sorted.len());
    for z in zones_sorted {
        let dead: HashSet<usize> = manifest
            .zones
            .iter()
            .enumerate()
            .filter(|(_, &zone)| zone == z)
            .map(|(i, _)| i)
            .collect();
        let mut resolution: Option<u8> = None;
        for l in 0..manifest.nlayers as usize {
            let mut all_ok = true;
            for c in 0..manifest.channels as usize {
                let alive = layout[c][l].iter().filter(|n| !dead.contains(n)).count() as u32;
                if alive < k {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                resolution = Some(l as u8);
            } else {
                break;
            }
        }
        out.push(ZoneFailure {
            zone: z,
            nodes_in_zone: dead.len() as u32,
            resolution,
        });
    }
    out
}

// === Internal ==============================================================

fn sorted(live: &LiveNodes) -> Vec<usize> {
    let mut v = live.clone();
    v.sort();
    v
}

/// For each (channel, layer) — the list of nodes shards would land on via
/// HRW over `live_sorted`. Inner list length = `n_per_layer[layer]`.
fn build_layout(manifest: &Manifest, live_sorted: &[usize]) -> Vec<Vec<Vec<usize>>> {
    let mut layout = vec![vec![Vec::new(); manifest.nlayers as usize]; manifest.channels as usize];
    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            let n = manifest.n_per_layer[l as usize];
            let lay = &mut layout[c as usize][l as usize];
            lay.reserve(n as usize);
            for idx in 0..n {
                lay.push(manifest.place_shard(c, l, idx, live_sorted));
            }
        }
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use holofs_model::placement::Placement;

    fn manifest_2x2(n_per_layer: &[u32]) -> Manifest {
        let nlayers = n_per_layer.len() as u8;
        Manifest {
            object_id: 0x1234,
            k: 4,
            nlayers,
            n_per_layer: n_per_layer.to_vec(),
            sym_len: vec![32; nlayers as usize],
            layer_positions: vec![vec![]; nlayers as usize],
            channels: 2,
            width: 8,
            height: 8,
            levels: 1,
            nodes: (0..10).map(|i| format!("127.0.0.1:{i}")).collect(),
            placement: Placement::Rendezvous,
            zones: vec![0; 10],
            data_cid: [42; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); nlayers as usize]; 2],
            kind: holofs_model::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
        }
    }

    #[test]
    fn current_resolution_full() {
        let stats = vec![
            LayerStats {
                channel: 0,
                layer: 0,
                n_shards: 10,
                n_alive: 10,
                n_hosting_nodes: 8,
                k: 4,
                margin: 6,
            },
            LayerStats {
                channel: 1,
                layer: 0,
                n_shards: 10,
                n_alive: 10,
                n_hosting_nodes: 8,
                k: 4,
                margin: 6,
            },
            LayerStats {
                channel: 0,
                layer: 1,
                n_shards: 6,
                n_alive: 6,
                n_hosting_nodes: 5,
                k: 4,
                margin: 2,
            },
            LayerStats {
                channel: 1,
                layer: 1,
                n_shards: 6,
                n_alive: 6,
                n_hosting_nodes: 5,
                k: 4,
                margin: 2,
            },
        ];
        assert_eq!(current_resolution(&stats, 2, 2), Some(1));
    }

    #[test]
    fn current_resolution_drops_at_first_broken_layer() {
        // L0 ok, L1 broken in one channel.
        let mut stats = vec![
            LayerStats {
                channel: 0,
                layer: 0,
                n_shards: 10,
                n_alive: 10,
                n_hosting_nodes: 8,
                k: 4,
                margin: 6,
            },
            LayerStats {
                channel: 1,
                layer: 0,
                n_shards: 10,
                n_alive: 10,
                n_hosting_nodes: 8,
                k: 4,
                margin: 6,
            },
            LayerStats {
                channel: 0,
                layer: 1,
                n_shards: 6,
                n_alive: 6,
                n_hosting_nodes: 5,
                k: 4,
                margin: 2,
            },
            LayerStats {
                channel: 1,
                layer: 1,
                n_shards: 6,
                n_alive: 2,
                n_hosting_nodes: 2,
                k: 4,
                margin: -2,
            },
        ];
        assert_eq!(current_resolution(&stats, 2, 2), Some(0));
        // And if L0 also fails — None.
        stats[0].n_alive = 1;
        stats[0].margin = -3;
        assert_eq!(current_resolution(&stats, 2, 2), None);
    }

    #[test]
    fn simulate_no_kill_is_perfect_health() {
        let m = manifest_2x2(&[10, 6]);
        let live: Vec<usize> = (0..10).collect();
        let mut rng = Rng::new(1);
        let scen = simulate_loss(&m, &live, &[0], 100, &mut rng);
        // kill_pct=0 → 100% of trials give full resolution.
        // dist is indexed from "dead (0)" up to "all layers (nlayers)".
        assert_eq!(scen[0].dist.len(), m.nlayers as usize + 1);
        assert_eq!(scen[0].dist[m.nlayers as usize], 100);
        for i in 0..m.nlayers as usize {
            assert_eq!(scen[0].dist[i], 0);
        }
    }

    #[test]
    fn simulate_full_kill_is_total_failure() {
        let m = manifest_2x2(&[10, 6]);
        let live: Vec<usize> = (0..10).collect();
        let mut rng = Rng::new(1);
        let scen = simulate_loss(&m, &live, &[100], 50, &mut rng);
        // kill_pct=100 → all nodes dead, no layer survives.
        assert_eq!(scen[0].dist[0], 50);
        for i in 1..=m.nlayers as usize {
            assert_eq!(scen[0].dist[i], 0);
        }
    }

    #[test]
    fn simulate_loss_is_monotone_in_kill_pct() {
        // The bigger the %, the worse the average "layers survived".
        let m = manifest_2x2(&[20, 16, 10, 8]);
        let live: Vec<usize> = (0..10).collect();
        let mut rng = Rng::new(7);
        let scen = simulate_loss(&m, &live, &[10, 30, 60, 90], 1000, &mut rng);
        let avg = |s: &LossScenario| -> f32 {
            let n = s.n_trials as f32;
            // dist[0]=dead (-1), dist[1]=L0 (0), ..., weighted sum
            let mut sum = 0f32;
            for (i, &c) in s.dist.iter().enumerate() {
                sum += (i as f32 - 1.0) * (c as f32);
            }
            sum / n
        };
        let a = avg(&scen[0]);
        let b = avg(&scen[1]);
        let c = avg(&scen[2]);
        let d = avg(&scen[3]);
        assert!(
            a >= b && b >= c && c >= d,
            "monotonicity violated: {a} {b} {c} {d}"
        );
    }

    #[test]
    fn zone_failures_empty_for_single_zone_cluster() {
        let m = manifest_2x2(&[10, 6]);
        let live: Vec<usize> = (0..10).collect();
        // By default zones = vec![0; 10] from manifest_2x2 — a single zone.
        let zf = simulate_zone_failures(&m, &live);
        assert!(zf.is_empty());
    }

    #[test]
    fn zone_failures_reflect_anti_affinity() {
        // Manifest on 16 nodes in 4 zones with zone-aware placement, big margin.
        let mut m = manifest_2x2(&[16, 16]);
        m.nodes = (0..16).map(|i| format!("127.0.0.1:{i}")).collect();
        m.zones = (0..16u8).map(|i| i / 4).collect();
        m.placement = Placement::RendezvousZoneAware;
        m.k = 4;
        let live: Vec<usize> = (0..16).collect();
        let zf = simulate_zone_failures(&m, &live);
        assert_eq!(zf.len(), 4, "one scenario per zone");
        // 16 shards over 4 zones → 4 per zone. Lose one → 12 alive ≥ K=4.
        for f in &zf {
            assert_eq!(
                f.resolution,
                Some(1),
                "zone-aware must survive a zone failure"
            );
        }
    }

    #[test]
    fn pmf_sums_to_one() {
        let m = manifest_2x2(&[12, 8]);
        let live: Vec<usize> = (0..10).collect();
        let mut rng = Rng::new(123);
        let scen = simulate_loss(&m, &live, &[40], 200, &mut rng);
        let p = scen[0].pmf();
        let sum: f32 = p.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "pmf does not sum to 1: {sum}");
    }
}
