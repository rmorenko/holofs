//! perceptual fingerprints and cross-object analytics on top of shards.
//!
//! Idea: systematic L0 shards contain **raw bytes of the DWT LL band** as
//! f32 LE. That is effectively "low resolution in the frequency domain" —
//! exactly what perceptual hashes (pHash/dHash) exist for.
//!
//! The 16-byte fingerprint is the per-byte mean of each of the K=16
//! systematic L0 shards, clamped to 0..255. For a 512×512 image after a
//! 3-level DWT this yields a **4×4 grid of mean luminances** — classic dHash.
//!
//! ## Two similarity metrics
//!
//! The fingerprint **bytes** themselves are kept as u8 means — they are
//! useful for the `/api/fingerprint/<name>` JSON view and for crude
//! "is this exactly the same picture" checks. But the byte-magnitude L1
//! distance saturates fast on natural images (typical inter-image
//! distance ≈ 50-400 out of a theoretical max of FP_LEN × 255), so
//! percentage scores all cluster at 85–99% and stop being informative.
//!
//! For the `/similar/<name>` UI we therefore use classic **dHash**:
//! each pair of adjacent tile means produces one bit (`fp[i] > fp[i+1]`),
//! computed independently within every channel's strip and concatenated.
//! With 3 channels × 16 means → 3 × 15 = 45 bits. Hamming distance over
//! those bits distinguishes geometric transforms — a 180°-flipped image
//! picks up many flipped bits in each channel's gradient — and chroma
//! divergence (e.g. a colorful photo vs a mostly-monochrome mandala)
//! shows up strongly in the green/blue strips. The percentage is
//! `1 - hamming / 45`.
//!
//! ## Channel layout in the fingerprint
//!
//! Bytes are laid out per-channel in source order: `[R0..R15, G0..G15,
//! B0..B15]` for 3-channel images. For 1-channel audio only the first
//! 16 bytes are populated; the rest stay at 0. For text/opaque we fall
//! back to `data_cid` repeated to fill 48 bytes — a meaningless
//! "perceptual" hash but stable across PUTs of the same payload.

use holofs_core::merkle::Hash;
use holofs_core::rlnc::Shard;
use holofs_model::manifest::{Manifest, ObjectKind};

/// Number of mean-byte slots reserved per channel inside [`Fingerprint`].
pub const FP_PER_CHANNEL: usize = 16;
/// Total length of the perceptual fingerprint in bytes. 3 channels ×
/// 16 means each = 48 bytes; 1-channel kinds use only the first 16.
pub const FP_LEN: usize = FP_PER_CHANNEL * 3;

pub type Fingerprint = [u8; FP_LEN];

/// Compute a fingerprint from the K systematic L0 shards of every
/// channel. `shards_per_channel[c]` carries the verified L0 shards for
/// channel `c`; pass an empty `Vec` for channels not yet known. Channels
/// past index 2 are ignored (the fingerprint covers RGB only).
///
/// For each channel we scan its shards, take the K identity-coded
/// (systematic) ones, and store their per-payload mean into the
/// corresponding 16-byte strip. Missing positions stay 0. For text /
/// opaque kinds we fall back to `data_cid` repeated to fill 48 bytes.
pub fn perceptual_fingerprint(
    manifest: &Manifest,
    shards_per_channel: &[Vec<Shard>],
) -> Fingerprint {
    let mut fp = [0u8; FP_LEN];
    if manifest.kind == ObjectKind::Opaque || manifest.kind == ObjectKind::Text {
        // CID is 32 bytes; we need 48. Repeat the first 16 bytes to fill.
        fp[..32].copy_from_slice(&manifest.data_cid[..32]);
        fp[32..].copy_from_slice(&manifest.data_cid[..16]);
        return fp;
    }
    let k = manifest.k as usize;
    for (c, shards) in shards_per_channel.iter().enumerate().take(3) {
        let base = c * FP_PER_CHANNEL;
        for s in shards {
            if let Some(i) = identity_index(&s.coeffs) {
                if i < FP_PER_CHANNEL && i < k {
                    fp[base + i] = byte_mean(&s.payload);
                }
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
/// 0 = identical. Maximum = 16 * 255 = 4080. Kept for the JSON
/// `fingerprint` API and as a raw secondary number on the `/similar`
/// table; the UI similarity-pct comes from [`fingerprint_similarity_pct`]
/// (dHash + Hamming) instead.
pub fn fingerprint_distance(a: &Fingerprint, b: &Fingerprint) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .sum()
}

/// Total number of dHash bits the fingerprint produces. 3 strips of
/// `FP_PER_CHANNEL - 1` = 3 × 15 = 45.
pub const DHASH_BITS: u32 = ((FP_PER_CHANNEL - 1) * 3) as u32;

/// dHash bits derived from the byte-mean fingerprint. Each bit answers
/// "is tile i brighter than tile i+1", computed **independently within
/// each channel strip** — we don't compare across channel boundaries
/// because the means are channel-relative, not directly comparable.
///
/// Returns 45 bits packed into the low half of a u64; the rest is 0.
pub fn dhash_bits(fp: &Fingerprint) -> u64 {
    let mut bits: u64 = 0;
    let mut pos: u32 = 0;
    for c in 0..3 {
        let base = c * FP_PER_CHANNEL;
        for i in 0..FP_PER_CHANNEL - 1 {
            if fp[base + i] > fp[base + i + 1] {
                bits |= 1u64 << pos;
            }
            pos += 1;
        }
    }
    bits
}

/// Hamming distance over the 45-bit dHash representation. Range 0..=45.
pub fn fingerprint_hamming(a: &Fingerprint, b: &Fingerprint) -> u32 {
    (dhash_bits(a) ^ dhash_bits(b)).count_ones()
}

/// Similarity in percent, **re-anchored against the random-baseline**.
/// Two unrelated 45-bit hashes statistically share ~22.5 bits already
/// just by chance, so the naive `(1 - h/45) * 100` placed the noise
/// floor at 50% — making everything look weakly similar. We map
/// `0..=half` Hamming linearly to `100..=0` and clamp anything above
/// the midpoint to 0; the new scale reads as a real "signal above
/// noise":
///
/// | Hamming | similarity |
/// |--------:|-----------:|
/// |       0 |       100% |
/// |       3 |       ~73% |
/// |      11 |       ~51% |
/// |      18 |       ~20% |
/// |    ≥ 23 |         0% |
///
/// Identical fingerprints → 100, mid-range Hamming ≈ noise → 0, and
/// genuinely similar inputs (blurred / desaturated copies, etc.) land
/// in the 40–90 band where humans can actually rank them.
pub fn fingerprint_similarity_pct(a: &Fingerprint, b: &Fingerprint) -> f32 {
    let h = fingerprint_hamming(a, b) as f32;
    let half = DHASH_BITS as f32 / 2.0;
    (100.0 * (1.0 - h / half)).clamp(0.0, 100.0)
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

/// per-layer shard overlap. Returns, for every layer in
/// the target manifest `a`, the number of `a`'s hashes that also live
/// in `b` (across any layer of `b`). The split lets the caller tell
/// "shares coarse structure" (overlap concentrated in low layers)
/// apart from "shares fine detail" (concentrated in high layers); the
/// difference is the robust-copy signal: watermarks / re-encodes
/// preserve low-layer hashes while perturbing the high ones.
pub fn shard_overlap_per_layer(a: &Manifest, b: &Manifest) -> Vec<u32> {
    use std::collections::HashSet;
    if a.nlayers == 0 {
        return Vec::new();
    }
    let mut set_b: HashSet<Hash> = HashSet::new();
    for per_c in &b.shard_hashes {
        for per_l in per_c {
            for h in per_l {
                set_b.insert(*h);
            }
        }
    }
    let mut per_layer = vec![0u32; a.nlayers as usize];
    for per_c in &a.shard_hashes {
        for (l, per_l) in per_c.iter().enumerate() {
            if l >= per_layer.len() {
                break;
            }
            for h in per_l {
                if set_b.contains(h) {
                    per_layer[l] += 1;
                }
            }
        }
    }
    per_layer
}

/// How many shard hashes overlap between two manifests. Returns
/// (common, total_a, total_b). For identical objects (same CID, deterministic
/// PUT) common = total_a = total_b.
///
/// v2 P1.7: `total_a` is `set_a.len()` (unique count), so `total_b` and
/// `common` must also be counted post-dedup on `b`'s side. Prior code
/// counted `total_b` and `common` with per-shard multiplicity, making
/// the ratio `common / total_a` misleading (numerator/denominator on
/// different scales) whenever a manifest contained duplicate hashes.
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
    let mut set_b: HashSet<Hash> = HashSet::new();
    for per_c in &b.shard_hashes {
        for per_l in per_c {
            for h in per_l {
                set_b.insert(*h);
            }
        }
    }
    let common = set_a.intersection(&set_b).count();
    (common, set_a.len(), set_b.len())
}

// === Per-chunk diff ============================================

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
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
            state: holofs_model::manifest::ManifestState::Ready,
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

    /// 3-channel shard set with the same per-channel pattern. For
    /// fingerprint tests this gives a stable 48-byte output that mirrors
    /// the production code path.
    fn per_channel_systematic(pattern: &[u8]) -> Vec<Vec<Shard>> {
        let mut out = Vec::with_capacity(3);
        for _ in 0..3 {
            out.push(
                pattern
                    .iter()
                    .enumerate()
                    .map(|(i, &b)| sys_shard(i, b))
                    .collect(),
            );
        }
        out
    }

    #[test]
    fn fingerprint_identical_data_gives_zero_distance() {
        let m = fake_manifest(ObjectKind::Image, 1);
        let pattern: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let shards = per_channel_systematic(&pattern);
        let fp1 = perceptual_fingerprint(&m, &shards);
        let fp2 = perceptual_fingerprint(&m, &shards);
        assert_eq!(fingerprint_distance(&fp1, &fp2), 0);
        assert_eq!(fingerprint_similarity_pct(&fp1, &fp2), 100.0);
    }

    #[test]
    fn fingerprint_picks_up_brightness_per_chunk() {
        let m = fake_manifest(ObjectKind::Image, 1);
        let pattern: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let shards = per_channel_systematic(&pattern);
        let fp = perceptual_fingerprint(&m, &shards);
        // Channel 0 strip
        for i in 0..16 {
            assert_eq!(fp[i], (i * 10) as u8);
        }
        // Channel 1 + 2 strips carry the same pattern.
        for c in 1..3 {
            for i in 0..16 {
                assert_eq!(fp[c * FP_PER_CHANNEL + i], (i * 10) as u8);
            }
        }
    }

    #[test]
    fn fingerprint_ignores_rlnc_shards() {
        let m = fake_manifest(ObjectKind::Image, 1);
        // For each channel: 8 systematic + 8 RLNC shards.
        let mut shards: Vec<Shard> = (0..8).map(|i| sys_shard(i, 100)).collect();
        for _ in 0..8 {
            shards.push(Shard {
                coeffs: vec![0x55; 16],
                payload: vec![0xAA; 64],
            });
        }
        let per_channel = vec![shards.clone(), shards.clone(), shards];
        let fp = perceptual_fingerprint(&m, &per_channel);
        for c in 0..3 {
            let base = c * FP_PER_CHANNEL;
            for i in 0..8 {
                assert_eq!(fp[base + i], 100);
            }
            for i in 8..16 {
                assert_eq!(fp[base + i], 0, "RLNC shard must not enter ch {c} pos {i}");
            }
        }
    }

    #[test]
    fn l1_distance_grows_with_difference() {
        let mut a: Fingerprint = [0; FP_LEN];
        let mut b: Fingerprint = [0; FP_LEN];
        a[0] = 100;
        b[0] = 200;
        assert_eq!(fingerprint_distance(&a, &b), 100);
    }

    #[test]
    fn dhash_similarity_identical_is_100() {
        let mut fp: Fingerprint = [0; FP_LEN];
        for (i, b) in fp.iter_mut().enumerate() {
            *b = (i * 5) as u8;
        }
        assert_eq!(fingerprint_similarity_pct(&fp, &fp), 100.0);
        assert_eq!(fingerprint_hamming(&fp, &fp), 0);
    }

    #[test]
    fn dhash_similarity_noise_floor_is_zero() {
        // Random uncorrelated bit patterns should map to ≈ 0%, not 50%.
        // The midpoint h=22.5 is the statistical noise baseline for 45
        // bits; the re-anchored formula clamps anything ≥ that to 0%.
        let mut a: Fingerprint = [0; FP_LEN];
        let mut b: Fingerprint = [0; FP_LEN];
        // Construct: every other tile within each channel inverted →
        // many flipped dHash bits across all 3 strips.
        for i in 0..FP_LEN {
            a[i] = if i % 2 == 0 { 50 } else { 200 };
            b[i] = if i % 2 == 0 { 200 } else { 50 };
        }
        let s = fingerprint_similarity_pct(&a, &b);
        assert!(s < 5.0, "uncorrelated patterns should clamp to 0, got {s}");
    }

    #[test]
    fn dhash_similarity_reversed_drops_sharply() {
        // Reverse every channel strip individually — analogous to a
        // 180°-rotated image whose per-channel gradients all invert.
        let mut a: Fingerprint = [0; FP_LEN];
        for (i, b) in a.iter_mut().enumerate() {
            *b = (i % FP_PER_CHANNEL * 10) as u8;
        }
        let mut b = a;
        for c in 0..3 {
            b[c * FP_PER_CHANNEL..(c + 1) * FP_PER_CHANNEL].reverse();
        }
        let s = fingerprint_similarity_pct(&a, &b);
        assert!(s < 20.0, "reversed similarity must be low, got {s}");
    }

    #[test]
    fn dhash_similarity_unrelated_patterns_clamp_to_zero() {
        // With the re-anchored scale, two completely unrelated fingerprints
        // collapse to 0% — they share no genuine bit signal above the
        // random baseline.
        let mut a: Fingerprint = [0; FP_LEN];
        let mut b: Fingerprint = [0; FP_LEN];
        let xs_a: [u8; 16] = [50, 80, 70, 120, 90, 30, 200, 10, 60, 40, 180, 120, 90, 70, 30, 100];
        let xs_b: [u8; 16] = [10, 5, 200, 30, 90, 40, 80, 220, 15, 150, 70, 60, 40, 130, 10, 90];
        for c in 0..3 {
            a[c * FP_PER_CHANNEL..(c + 1) * FP_PER_CHANNEL].copy_from_slice(&xs_a);
            b[c * FP_PER_CHANNEL..(c + 1) * FP_PER_CHANNEL].copy_from_slice(&xs_b);
        }
        let s = fingerprint_similarity_pct(&a, &b);
        assert!(s < 30.0, "unrelated should approach 0%, got {s}");
    }

    #[test]
    fn opaque_fingerprint_falls_back_to_cid() {
        // CID is 32 bytes of the same value; the fallback pads the
        // 48-byte fingerprint as [cid(32) | cid[..16]]. With a uniform
        // CID that means the whole 48-byte buffer reads as that value.
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
        let target: Fingerprint = [100; FP_LEN];
        let cats = vec![
            ("near".to_string(), [101u8; FP_LEN]),  // distance 48
            ("far".to_string(), [200u8; FP_LEN]),   // distance 4800
            ("self".to_string(), [100u8; FP_LEN]),  // filtered out
            ("close".to_string(), [105u8; FP_LEN]), // distance 240
        ];
        let r = rank_neighbors(&target, &cats, "self", 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, "near");
        assert_eq!(r[1].0, "close");
        assert_eq!(r[2].0, "far");
    }
}
