//! Stage 10: perceptual fingerprints and cross-object analytics on top of shards.
//!
//! Idea: systematic L0 shards contain **raw bytes of the DWT LL band** as
//! f32 LE. That is effectively "low resolution in the frequency domain" —
//! exactly what perceptual hashes (pHash/dHash) exist for.
//!
//! The 16-byte fingerprint is the per-byte mean of each of the K=16
//! systematic L0 shards, clamped to 0..255. For a 512×512 image after a
//! 3-level DWT this yields a **4×4 grid of mean luminances** — classic dHash.
//!
//! Distance is L1 over u8 (sum of |a-b|). We do not use Hamming because the
//! u8 means carry ordinal information, not binary bits.

use holofs_core::merkle::Hash;
use holofs_core::rlnc::Shard;
use holofs_model::manifest::{Manifest, ObjectKind};

/// Length of the perceptual fingerprint in bytes.
pub const FP_LEN: usize = 16;

pub type Fingerprint = [u8; FP_LEN];

/// Compute a fingerprint from the K systematic L0 shards of the first channel.
///
/// `shards_l0` — every known shard (channel=0, layer=0) of this object.
/// The function filters out systematic shards (identity coeffs) and takes
/// the first K. If fewer than K are available the fingerprint is computed
/// over what we have; remaining positions are 0. For image/audio this works
/// — the count of systematic shards is always stable.
pub fn perceptual_fingerprint(manifest: &Manifest, shards_l0: &[Shard]) -> Fingerprint {
    if manifest.kind == ObjectKind::Opaque || manifest.kind == ObjectKind::Text {
        // For opaque/text a perceptual fingerprint makes no physical sense —
        // these are raw bytes, not "low resolution". Return data_cid as a marker.
        let mut fp = [0u8; FP_LEN];
        fp.copy_from_slice(&manifest.data_cid[..FP_LEN]);
        return fp;
    }
    let mut fp = [0u8; FP_LEN];
    let k = manifest.k as usize;
    // Systematic shards have coeffs = e_i. Walk every available shard; for
    // each identity index in 0..K take the payload mean (as a byte).
    for s in shards_l0 {
        if let Some(i) = identity_index(&s.coeffs) {
            if i < FP_LEN && i < k {
                fp[i] = byte_mean(&s.payload);
            }
        }
    }
    fp
}

fn identity_index(coeffs: &[u8]) -> Option<usize> {
    let mut found = None;
    for (i, &c) in coeffs.iter().enumerate() {
        if c == 1 {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        } else if c != 0 {
            return None;
        }
    }
    found
}

/// Byte mean clamped to u8. The payload may be large (thousands of bytes);
/// we care about overall "energy" — that is the low-frequency information.
fn byte_mean(payload: &[u8]) -> u8 {
    if payload.is_empty() {
        return 0;
    }
    let sum: u64 = payload.iter().map(|&b| b as u64).sum();
    (sum / payload.len() as u64).min(255) as u8
}

/// L1 distance between fingerprints (sum of |a-b|). Smaller is more similar.
/// 0 = identical. Maximum = 16 * 255 = 4080.
pub fn fingerprint_distance(a: &Fingerprint, b: &Fingerprint) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .sum()
}

/// Similarity in percent (100 = identical, 0 = maximally different).
pub fn fingerprint_similarity_pct(a: &Fingerprint, b: &Fingerprint) -> f32 {
    let d = fingerprint_distance(a, b) as f32;
    let max = (FP_LEN * 255) as f32;
    ((1.0 - d / max) * 100.0).clamp(0.0, 100.0)
}

/// Hex representation of a fingerprint (32 hex chars).
pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    let mut s = String::with_capacity(FP_LEN * 2);
    for b in fp {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// === Cross-object analytics ===============================================

/// How many shard hashes overlap between two manifests. Returns
/// (common, total_a, total_b). For identical objects (same CID, deterministic
/// PUT) common = total_a = total_b.
pub fn shard_overlap(a: &Manifest, b: &Manifest) -> (usize, usize, usize) {
    use std::collections::HashSet;
    let mut set_a: HashSet<Hash> = HashSet::new();
    for per_c in &a.shard_hashes {
        for per_l in per_c {
            for h in per_l {
                set_a.insert(*h);
            }
        }
    }
    let total_a = set_a.len();
    let mut total_b = 0usize;
    let mut common = 0usize;
    for per_c in &b.shard_hashes {
        for per_l in per_c {
            for h in per_l {
                total_b += 1;
                if set_a.contains(h) {
                    common += 1;
                }
            }
        }
    }
    (common, total_a, total_b)
}

// === Per-chunk diff (Stage 10c) ============================================

/// One chunk-diff entry: where the chunk lives and whether it is shared
/// with the other object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDiffEntry {
    pub channel: u8,
    pub layer: u8,
    pub idx: u32,
    pub is_common: bool,
}

/// Result of a chunk-level comparison between two manifests. Systematic
/// shards (`shard_idx < K`) map unambiguously to source-data positions, so
/// a hash match = a byte-for-byte chunk match across both files. This is
/// **content-defined diff for free**: the dedup already computed those
/// hashes at PUT time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDiff {
    pub entries: Vec<ChunkDiffEntry>,
    pub common: usize,
    pub total: usize,
}

impl ChunkDiff {
    pub fn similarity_pct(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        self.common as f32 * 100.0 / self.total as f32
    }
}

/// Compare two manifests by systematic shards (first K per layer).
/// Returns per-chunk flags (common/different) + aggregated counters.
pub fn chunk_diff(a: &Manifest, b: &Manifest) -> ChunkDiff {
    let mut entries = Vec::new();
    let mut common = 0usize;
    let mut total = 0usize;

    let max_c = a.channels.min(b.channels);
    let max_l = a.nlayers.min(b.nlayers);
    let k = a.k.min(b.k) as usize;

    for c in 0..max_c {
        for l in 0..max_l {
            let a_hashes = &a.shard_hashes[c as usize][l as usize];
            let b_hashes = &b.shard_hashes[c as usize][l as usize];
            let max_idx = k.min(a_hashes.len()).min(b_hashes.len());
            for idx in 0..max_idx {
                total += 1;
                let is_common = a_hashes[idx] == b_hashes[idx];
                if is_common {
                    common += 1;
                }
                entries.push(ChunkDiffEntry {
                    channel: c,
                    layer: l,
                    idx: idx as u32,
                    is_common,
                });
            }
        }
    }

    ChunkDiff {
        entries,
        common,
        total,
    }
}

/// Neighbours of an object in the catalog, sorted by perceptual proximity.
/// Returns `Vec<(name, distance, similarity_pct)>` excluding the object itself.
pub fn rank_neighbors(
    target_fp: &Fingerprint,
    catalog_fps: &[(String, Fingerprint)],
    self_name: &str,
    limit: usize,
) -> Vec<(String, u32, f32)> {
    let mut rated: Vec<(String, u32, f32)> = catalog_fps
        .iter()
        .filter(|(n, _)| n != self_name)
        .map(|(n, fp)| {
            let d = fingerprint_distance(target_fp, fp);
            let s = fingerprint_similarity_pct(target_fp, fp);
            (n.clone(), d, s)
        })
        .collect();
    rated.sort_by_key(|(_, d, _)| *d);
    rated.truncate(limit);
    rated
}

#[cfg(test)]
mod tests {
    use super::*;
    use holofs_model::placement::Placement;

    fn fake_manifest(kind: ObjectKind, data_cid: u8) -> Manifest {
        Manifest {
            object_id: data_cid as u64,
            k: 16,
            nlayers: 4,
            n_per_layer: vec![16, 16, 16, 16],
            sym_len: vec![64; 4],
            layer_positions: vec![vec![]; 4],
            channels: 3,
            width: 512,
            height: 512,
            levels: 3,
            nodes: vec![],
            placement: Placement::Rendezvous,
            zones: vec![],
            data_cid: [data_cid; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 4]; 3],
            kind,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
        }
    }

    fn sys_shard(i: usize, payload_byte: u8) -> Shard {
        let mut coeffs = vec![0u8; 16];
        coeffs[i] = 1;
        Shard {
            coeffs,
            payload: vec![payload_byte; 64],
        }
    }

    #[test]
    fn fingerprint_identical_data_gives_zero_distance() {
        let m = fake_manifest(ObjectKind::Image, 1);
        let shards: Vec<Shard> = (0..16).map(|i| sys_shard(i, (i * 10) as u8)).collect();
        let fp1 = perceptual_fingerprint(&m, &shards);
        let fp2 = perceptual_fingerprint(&m, &shards);
        assert_eq!(fingerprint_distance(&fp1, &fp2), 0);
        assert_eq!(fingerprint_similarity_pct(&fp1, &fp2), 100.0);
    }

    #[test]
    fn fingerprint_picks_up_brightness_per_chunk() {
        let m = fake_manifest(ObjectKind::Image, 1);
        // The i-th chunk has bytes i*10.
        let shards: Vec<Shard> = (0..16).map(|i| sys_shard(i, (i * 10) as u8)).collect();
        let fp = perceptual_fingerprint(&m, &shards);
        for i in 0..16 {
            assert_eq!(fp[i], (i * 10) as u8);
        }
    }

    #[test]
    fn fingerprint_ignores_rlnc_shards() {
        let m = fake_manifest(ObjectKind::Image, 1);
        // 8 systematic + 8 RLNC shards (non-identity coeffs).
        let mut shards: Vec<Shard> = (0..8).map(|i| sys_shard(i, 100)).collect();
        for _ in 0..8 {
            shards.push(Shard {
                coeffs: vec![0x55; 16],
                payload: vec![0xAA; 64],
            });
        }
        let fp = perceptual_fingerprint(&m, &shards);
        for i in 0..8 {
            assert_eq!(fp[i], 100);
        }
        for i in 8..16 {
            assert_eq!(fp[i], 0, "an RLNC shard must not enter the fingerprint");
        }
    }

    #[test]
    fn distance_grows_with_difference() {
        let mut a: Fingerprint = [0; 16];
        let mut b: Fingerprint = [0; 16];
        a[0] = 100;
        b[0] = 200;
        assert_eq!(fingerprint_distance(&a, &b), 100);
        let s = fingerprint_similarity_pct(&a, &b);
        assert!(s > 95.0 && s < 100.0, "similarity {s}");
    }

    #[test]
    fn opaque_fingerprint_falls_back_to_cid() {
        let m = fake_manifest(ObjectKind::Opaque, 0xAB);
        let fp = perceptual_fingerprint(&m, &[]);
        assert_eq!(fp, [0xAB; FP_LEN]);
    }

    #[test]
    fn shard_overlap_identical_manifests() {
        let mut m = fake_manifest(ObjectKind::Image, 1);
        m.shard_hashes[0][0] = vec![[1; 32], [2; 32], [3; 32]];
        m.shard_hashes[1][0] = vec![[4; 32], [5; 32]];
        let (common, ta, tb) = shard_overlap(&m, &m);
        assert_eq!(common, 5);
        assert_eq!(ta, 5);
        assert_eq!(tb, 5);
    }

    #[test]
    fn shard_overlap_partial_intersection() {
        let mut a = fake_manifest(ObjectKind::Image, 1);
        a.shard_hashes[0][0] = vec![[1; 32], [2; 32], [3; 32]];
        let mut b = fake_manifest(ObjectKind::Image, 2);
        b.shard_hashes[0][0] = vec![[2; 32], [3; 32], [9; 32]];
        let (common, ta, tb) = shard_overlap(&a, &b);
        assert_eq!(common, 2); // [2;32] and [3;32]
        assert_eq!(ta, 3);
        assert_eq!(tb, 3);
    }

    #[test]
    fn chunk_diff_identical_manifests_all_common() {
        let mut m = fake_manifest(ObjectKind::Image, 1);
        // Fill shard_hashes with identical hashes in the first K positions.
        for c in 0..3 {
            for l in 0..4 {
                m.shard_hashes[c][l] = (0..16).map(|i| [i as u8; 32]).collect();
            }
        }
        let d = chunk_diff(&m, &m);
        assert_eq!(d.total, 3 * 4 * 16);
        assert_eq!(d.common, d.total);
        assert_eq!(d.similarity_pct(), 100.0);
    }

    #[test]
    fn chunk_diff_partial_overlap_counts_correctly() {
        let mut a = fake_manifest(ObjectKind::Text, 1);
        a.channels = 1;
        a.nlayers = 1;
        a.k = 4;
        a.shard_hashes = vec![vec![vec![[1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]]]];
        let mut b = fake_manifest(ObjectKind::Text, 2);
        b.channels = 1;
        b.nlayers = 1;
        b.k = 4;
        // 2 shared chunks (1 and 3), 2 different (5 and 6)
        b.shard_hashes = vec![vec![vec![[1u8; 32], [5u8; 32], [3u8; 32], [6u8; 32]]]];
        let d = chunk_diff(&a, &b);
        assert_eq!(d.total, 4);
        assert_eq!(d.common, 2);
        assert_eq!(d.similarity_pct(), 50.0);
        assert!(d.entries[0].is_common);
        assert!(!d.entries[1].is_common);
        assert!(d.entries[2].is_common);
        assert!(!d.entries[3].is_common);
    }

    #[test]
    fn rank_neighbors_sorts_by_distance() {
        let target: Fingerprint = [100; 16];
        let cats = vec![
            ("near".to_string(), [101u8; 16]),  // distance 16
            ("far".to_string(), [200u8; 16]),   // distance 1600
            ("self".to_string(), [100u8; 16]),  // filtered out
            ("close".to_string(), [105u8; 16]), // distance 80
        ];
        let r = rank_neighbors(&target, &cats, "self", 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, "near");
        assert_eq!(r[1].0, "close");
        assert_eq!(r[2].0, "far");
    }
}
