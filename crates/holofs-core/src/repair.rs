//! Stage 1: RLNC node regeneration.
//!
//! A new/replaced node fetches d live shards of a layer from peers and bakes
//! fresh shards as random linear combinations of those donors. File-level
//! decoding is skipped — CPU savings.
//!
//! Correctness condition: with d ≥ K and linearly-independent donors the new
//! shards span the full K-dimensional source space, so decodability is preserved.

use crate::cluster::ShardRec;
use crate::gf::Gf;
use crate::rlnc::Shard;
use crate::rng::Rng;
use crate::{K, NLAYERS};

/// Pure operation: "mix `need` new shards from `donors`".
/// Each new shard = Σ alpha_i · donor_i with random alpha over GF(256).
/// Used by both local [`regenerate_node`] and the distributed repair path.
pub fn mix_donors(gf: &Gf, donors: &[Shard], need: usize, rng: &mut Rng) -> Vec<Shard> {
    if donors.is_empty() || need == 0 {
        return Vec::new();
    }
    let slen = donors[0].payload.len();
    let mut out = Vec::with_capacity(need);
    for _ in 0..need {
        let mut nc = vec![0u8; K];
        let mut np = vec![0u8; slen];
        for src in donors {
            let alpha = rng.byte();
            if alpha == 0 {
                continue;
            }
            for i in 0..K {
                nc[i] ^= gf.mul(alpha, src.coeffs[i]);
            }
            for j in 0..slen {
                np[j] ^= gf.mul(alpha, src.payload[j]);
            }
        }
        if nc.iter().all(|&b| b == 0) {
            let src = &donors[0];
            nc.copy_from_slice(&src.coeffs);
            np.copy_from_slice(&src.payload);
        }
        out.push(Shard {
            coeffs: nc,
            payload: np,
        });
    }
    out
}

#[derive(Default, Debug)]
pub struct RepairStats {
    pub bytes_downloaded: u64,
    pub bytes_baseline_full: u64,
    pub gf_muls_repair: u64,
    pub gf_muls_baseline: u64,
    pub shards_generated: usize,
    pub layers_repaired: usize,
    pub layers_unrecoverable: usize,
}

/// Refill node `node_id` up to the target `n_per_layer[l]` shards for each
/// (channel, layer). `d` — how many live shards the node downloads per layer.
pub fn regenerate_node(
    gf: &Gf,
    node_id: usize,
    all: &mut Vec<ShardRec>,
    sym_len: &[usize; NLAYERS],
    n_per_layer: &[usize; NLAYERS],
    d: usize,
    rng: &mut Rng,
) -> RepairStats {
    let mut stats = RepairStats::default();
    all.retain(|r| r.node != node_id);

    let mut live_count = [[0usize; NLAYERS]; 3];
    for r in all.iter() {
        live_count[r.channel][r.layer] += 1;
    }

    for c in 0..3 {
        for l in 0..NLAYERS {
            let need = n_per_layer[l].saturating_sub(live_count[c][l]);
            if need == 0 {
                continue;
            }

            let donor_count = all
                .iter()
                .filter(|r| r.channel == c && r.layer == l)
                .count();

            if donor_count == 0 {
                stats.layers_unrecoverable += 1;
                continue;
            }

            let take = d.min(donor_count);
            let slen = sym_len[l];
            let bytes_per_shard = (K + slen) as u64;

            // Repair traffic: download `take` shards.
            stats.bytes_downloaded += take as u64 * bytes_per_shard;
            // Baseline scenario — full-layer reconstruction: K shards on the wire,
            // plus Gauss-Jordan and re-encoding `need` shards.
            let k_used = (K as u64).min(donor_count as u64);
            stats.bytes_baseline_full += k_used * bytes_per_shard;
            stats.gf_muls_baseline += (K as u64) * (K as u64 + 1) * (slen as u64 + K as u64)
                + (need as u64) * (K as u64) * (slen as u64);

            // "Download" — take `take` donor shards as owned copies.
            let sources: Vec<Shard> = all
                .iter()
                .filter(|r| r.channel == c && r.layer == l)
                .take(take)
                .map(|r| r.shard.clone())
                .collect();
            let muls_per_new = (take as u64) * (slen as u64 + K as u64);
            stats.gf_muls_repair += (need as u64) * muls_per_new;

            let fresh = mix_donors(gf, &sources, need, rng);
            for shard in fresh {
                all.push(ShardRec {
                    node: node_id,
                    channel: c,
                    layer: l,
                    shard,
                });
                stats.shards_generated += 1;
            }
            stats.layers_repaired += 1;
        }
    }
    stats
}

/// Trigger policy: returns true if for any (channel, layer) the surplus of
/// live shards above K has fallen below `threshold` times the original surplus
/// `n_per_layer[l] - K`. Layers with zero surplus are ignored.
pub fn needs_repair(all: &[ShardRec], n_per_layer: &[usize; NLAYERS], threshold: f32) -> bool {
    let mut live = [[0usize; NLAYERS]; 3];
    for r in all {
        live[r.channel][r.layer] += 1;
    }
    for c in 0..3 {
        for l in 0..NLAYERS {
            let full_surplus = n_per_layer[l] as i32 - K as i32;
            if full_surplus <= 0 {
                continue;
            }
            let surplus = live[c][l] as i32 - K as i32;
            if (surplus as f32) < threshold * (full_surplus as f32) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rlnc::{decode_layer, encode_layer};

    fn populate_layer(
        gf: &Gf,
        rng: &mut Rng,
        all: &mut Vec<ShardRec>,
        channel: usize,
        layer: usize,
        data: &[u8],
        n: usize,
        node_start: usize,
    ) -> usize {
        let (sl, shards) = encode_layer(gf, data, n, rng);
        for (i, shard) in shards.into_iter().enumerate() {
            all.push(ShardRec {
                node: node_start + i,
                channel,
                layer,
                shard,
            });
        }
        sl
    }

    fn truncate(data_len: usize, decoded: Vec<u8>) -> Vec<u8> {
        decoded.into_iter().take(data_len).collect()
    }

    #[test]
    fn regen_restores_layer_count_to_target() {
        let gf = Gf::new();
        let mut rng = Rng::new(11);
        let data = vec![42u8; K * 4];
        let n = 3 * K;
        let mut all: Vec<ShardRec> = Vec::new();
        let sl = populate_layer(&gf, &mut rng, &mut all, 0, 0, &data, n, 0);

        let sym_len = {
            let mut a = [0; NLAYERS];
            a[0] = sl;
            a
        };
        let npl = {
            let mut a = [0; NLAYERS];
            a[0] = n;
            a
        };

        let before = all.iter().filter(|r| r.node == 5).count();
        assert!(before > 0);
        let stats = regenerate_node(&gf, 5, &mut all, &sym_len, &npl, K, &mut rng);
        assert_eq!(stats.shards_generated, before);
        let live = all
            .iter()
            .filter(|r| r.channel == 0 && r.layer == 0)
            .count();
        assert_eq!(live, n, "layer refilled up to the target n");
    }

    #[test]
    fn regen_preserves_decodability() {
        let gf = Gf::new();
        let mut rng = Rng::new(99);
        let data: Vec<u8> = (0..K * 30).map(|i| (i as u8).wrapping_mul(31)).collect();
        let n = 2 * K;
        let mut all: Vec<ShardRec> = Vec::new();
        let sl = populate_layer(&gf, &mut rng, &mut all, 0, 0, &data, n, 0);

        let sym_len = {
            let mut a = [0; NLAYERS];
            a[0] = sl;
            a
        };
        let npl = {
            let mut a = [0; NLAYERS];
            a[0] = n;
            a
        };

        // Series of "death → regeneration" at the same positions.
        for node in 0..10 {
            regenerate_node(&gf, node, &mut all, &sym_len, &npl, K, &mut rng);
        }

        // Any K shards must decode back to the source.
        let live: Vec<&Shard> = all
            .iter()
            .filter(|r| r.channel == 0 && r.layer == 0)
            .take(K)
            .map(|r| &r.shard)
            .collect();
        let decoded = decode_layer(&gf, &live, sl).expect("decode after regen");
        assert_eq!(truncate(data.len(), decoded), data);
    }

    #[test]
    fn regen_marks_unrecoverable_when_no_donors() {
        let gf = Gf::new();
        let mut rng = Rng::new(3);
        let mut all: Vec<ShardRec> = Vec::new();
        let data = vec![7u8; K];
        // Place shards on nodes 0..K-1, then wipe them entirely.
        let sl = populate_layer(&gf, &mut rng, &mut all, 0, 0, &data, K, 0);
        all.retain(|r| r.channel != 0 || r.layer != 0);

        let sym_len = {
            let mut a = [0; NLAYERS];
            a[0] = sl;
            a
        };
        let npl = {
            let mut a = [0; NLAYERS];
            a[0] = K;
            a
        };

        let stats = regenerate_node(&gf, 99, &mut all, &sym_len, &npl, K, &mut rng);
        // regenerate_node walks every (c, l) with npl > 0; here l=0 is set for
        // all 3 channels, but no shards exist anywhere → 3 pairs flagged unrecoverable.
        assert_eq!(stats.layers_unrecoverable, 3);
        assert_eq!(stats.shards_generated, 0);
    }

    #[test]
    fn regen_baseline_cpu_higher_than_repair() {
        // Main repair benefit: fewer GF multiplications than full reconstruction.
        let gf = Gf::new();
        let mut rng = Rng::new(77);
        let data: Vec<u8> = (0..K * 50).map(|i| i as u8).collect();
        let n = 2 * K;
        let mut all: Vec<ShardRec> = Vec::new();
        let sl = populate_layer(&gf, &mut rng, &mut all, 0, 0, &data, n, 0);

        let sym_len = {
            let mut a = [0; NLAYERS];
            a[0] = sl;
            a
        };
        let npl = {
            let mut a = [0; NLAYERS];
            a[0] = n;
            a
        };

        let stats = regenerate_node(&gf, 3, &mut all, &sym_len, &npl, K, &mut rng);
        assert!(
            stats.gf_muls_repair < stats.gf_muls_baseline,
            "repair ({}) must be cheaper than baseline ({})",
            stats.gf_muls_repair,
            stats.gf_muls_baseline
        );
    }

    #[test]
    fn needs_repair_threshold_triggers() {
        let gf = Gf::new();
        let mut rng = Rng::new(0);
        let data = vec![1u8; K];
        let mut all: Vec<ShardRec> = Vec::new();
        // Surplus per layer per channel = K (n = 2K, threshold K).
        let mut node_off = 0usize;
        for l in 0..NLAYERS {
            for c in 0..3 {
                populate_layer(&gf, &mut rng, &mut all, c, l, &data, 2 * K, node_off);
                node_off += 2 * K;
            }
        }
        let npl = [2 * K; NLAYERS];

        // Full complement — trigger does not fire.
        assert!(!needs_repair(&all, &npl, 0.5));

        // Drop 9 shards from (c=0, l=0): surplus 16 → 7 < 0.5 * 16 = 8.
        let mut removed = 0;
        all.retain(|r| {
            if r.channel == 0 && r.layer == 0 && removed < 9 {
                removed += 1;
                false
            } else {
                true
            }
        });
        assert!(needs_repair(&all, &npl, 0.5));
    }

    #[test]
    fn needs_repair_ignores_layers_without_surplus() {
        // If n_per_layer[l] ≤ K, full_surplus ≤ 0 — threshold is not applied.
        let gf = Gf::new();
        let mut rng = Rng::new(0);
        let data = vec![1u8; K];
        let mut all: Vec<ShardRec> = Vec::new();
        populate_layer(&gf, &mut rng, &mut all, 0, 0, &data, K, 0);

        let mut npl = [0; NLAYERS];
        npl[0] = K; // exactly the threshold — full_surplus = 0
        assert!(!needs_repair(&all, &npl, 0.99));
    }
}
