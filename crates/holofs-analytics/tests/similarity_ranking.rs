//! end-to-end property tests for the perceptual-similarity
//! pipeline.
//!
//! The unit tests inside `fingerprint.rs` cover individual helpers
//! (`dhash_bits`, `fingerprint_hamming`, …) on hand-crafted byte arrays.
//! This suite instead builds synthetic 3-channel manifests + shards
//! with **known perceptual relationships** and asserts that the full
//! pipeline — `perceptual_fingerprint` → `fingerprint_similarity_pct` →
//! `rank_neighbors` — preserves the relationships we'd expect from a
//! real catalog.
//!
//! Sample taxonomy (every one is a 3-channel image with K=16 systematic
//! L0 shards per channel; payload is a uniform `u8` slab so `byte_mean`
//! reads it back as a controlled brightness):
//!
//! - `gradient` — R/G/B each carry their own monotonic ramp; this is
//!   the reference object.
//! - `mild_blur` — every tile's mean shifted by ±1 (smoothing).
//! - `heavy_blur` — every mean shifted by ±10 (stronger smoothing).
//! - `desaturated` — G and B channels mirror R (luminance only).
//! - `flipped` — each channel's tile order is reversed (analogous to a
//!   180°-rotated copy).
//! - `random` — deterministic xorshift over all 48 bytes.
//!
//! Tests assert that the dHash-Hamming similarity, **re-anchored** to
//! the random baseline, ranks these in the expected order and clamps
//! truly unrelated pairs to ~0 %.

use holofs_analytics::fingerprint::{
    dhash_bits, fingerprint_distance, fingerprint_hamming, fingerprint_hex,
    fingerprint_similarity_pct, perceptual_fingerprint, rank_neighbors, Fingerprint, FP_LEN,
    FP_PER_CHANNEL,
};
use holofs_core::rlnc::Shard;
use holofs_model::manifest::{Manifest, ObjectKind};
use holofs_model::placement::Placement;

// ===== test fixtures =======================================================

/// Build a Manifest matching the shape of an image PUT: 3 channels,
/// 4 layers, K=16. `shard_hashes` are left empty — the fingerprint path
/// doesn't read them, and the perceptual layer never reaches placement.
fn image_manifest(object_id: u64) -> Manifest {
    Manifest {
        object_id,
        k: 16,
        nlayers: 4,
        n_per_layer: vec![16, 16, 16, 16],
        sym_len: vec![64; 4],
        layer_positions: vec![vec![]; 4],
        channels: 3,
        width: 64,
        height: 64,
        levels: 3,
        nodes: vec![],
        placement: Placement::Rendezvous,
        zones: vec![],
        data_cid: [0u8; 32],
        merkle_root: [0u8; 32],
        shard_hashes: vec![vec![Vec::new(); 4]; 3],
        kind: ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: 0,
        encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
    }
}

/// A systematic shard whose payload has a known byte-mean. We use a flat
/// `mean`-valued slab so `byte_mean` reads back exactly `mean`.
fn sys_shard(idx: usize, mean: u8) -> Shard {
    let mut coeffs = vec![0u8; 16];
    coeffs[idx] = 1;
    Shard {
        coeffs,
        payload: vec![mean; 64],
    }
}

/// Take 3 channels of u8 patterns (R / G / B) and produce a
/// `[channel][shards]` matrix ready for `perceptual_fingerprint`.
fn shards_from_patterns(channels: [&[u8; FP_PER_CHANNEL]; 3]) -> Vec<Vec<Shard>> {
    channels
        .iter()
        .map(|pat| {
            pat.iter()
                .enumerate()
                .map(|(i, &m)| sys_shard(i, m))
                .collect()
        })
        .collect()
}

/// Shortcut: compute fingerprint from three 16-byte channel patterns.
fn fp_from(channels: [&[u8; FP_PER_CHANNEL]; 3]) -> Fingerprint {
    let m = image_manifest(1);
    let shards = shards_from_patterns(channels);
    perceptual_fingerprint(&m, &shards)
}

/// Reference "image": a gentle gradient in each channel, with R/G/B
/// independent so chroma carries real signal.
fn pattern_gradient() -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut r = [0u8; FP_PER_CHANNEL];
    let mut g = [0u8; FP_PER_CHANNEL];
    let mut b = [0u8; FP_PER_CHANNEL];
    for i in 0..FP_PER_CHANNEL {
        r[i] = 60 + (i as u8) * 4;
        g[i] = 80 + ((FP_PER_CHANNEL as u8 - i as u8 - 1)) * 3;
        b[i] = 50 + ((i as u8) % 4) * 12 + ((i / 4) as u8) * 8;
    }
    [r, g, b]
}

/// Shift every tile by a constant delta; gradient order is preserved.
/// Mimics global brightness lift / mild blur (`delta = ±2`) or heavier
/// smoothing (`delta = ±10`).
fn pattern_shift(base: &[[u8; FP_PER_CHANNEL]; 3], delta: i16) -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut out = [[0u8; FP_PER_CHANNEL]; 3];
    for c in 0..3 {
        for i in 0..FP_PER_CHANNEL {
            out[c][i] = (base[c][i] as i16 + delta).clamp(0, 255) as u8;
        }
    }
    out
}

/// Smear adjacent tiles together — a real "blur" that perturbs gradient
/// ordering at boundaries. `kernel = 1` is a mild 3-tap moving average;
/// larger values smooth more aggressively.
fn pattern_blur(base: &[[u8; FP_PER_CHANNEL]; 3], radius: usize) -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut out = [[0u8; FP_PER_CHANNEL]; 3];
    for c in 0..3 {
        for i in 0..FP_PER_CHANNEL {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius + 1).min(FP_PER_CHANNEL);
            let mut sum = 0u32;
            for j in lo..hi {
                sum += base[c][j] as u32;
            }
            out[c][i] = (sum / (hi - lo) as u32) as u8;
        }
    }
    out
}

/// Make G and B mirror R — like a desaturated photo where chroma carries
/// no information. This destroys cross-channel dHash diversity and
/// should drop similarity sharply against any normal RGB image.
fn pattern_desaturate(base: &[[u8; FP_PER_CHANNEL]; 3]) -> [[u8; FP_PER_CHANNEL]; 3] {
    [base[0], base[0], base[0]]
}

/// Reverse the tile order within every channel — analogous to a 180°
/// rotated copy. dHash bits flip in each strip's gradient comparisons.
fn pattern_flip(base: &[[u8; FP_PER_CHANNEL]; 3]) -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut out = *base;
    for c in 0..3 {
        out[c].reverse();
    }
    out
}

/// xorshift64 byte stream — deterministic, no dev-deps.
fn random_pattern(mut seed: u64) -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut out = [[0u8; FP_PER_CHANNEL]; 3];
    for c in 0..3 {
        for i in 0..FP_PER_CHANNEL {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out[c][i] = (seed & 0xFF) as u8;
        }
    }
    out
}

// ===== Property tests ======================================================

#[test]
fn similarity_is_reflexive_for_many_samples() {
    // For every kind of fingerprint we throw at it, sim(x, x) must be
    // exactly 100. This protects against scaling regressions like the
    // earlier off-by-one denominator that gave 93.3 % for identicals.
    let samples = vec![
        fp_from([&pattern_gradient()[0], &pattern_gradient()[1], &pattern_gradient()[2]]),
        fp_from([
            &random_pattern(7)[0],
            &random_pattern(7)[1],
            &random_pattern(7)[2],
        ]),
        [0u8; FP_LEN],
        [255u8; FP_LEN],
        {
            let mut fp = [0u8; FP_LEN];
            for i in 0..FP_LEN {
                fp[i] = (i * 5) as u8;
            }
            fp
        },
    ];
    for fp in &samples {
        assert_eq!(
            fingerprint_similarity_pct(fp, fp),
            100.0,
            "self-similarity must be 100 for {}",
            fingerprint_hex(fp)
        );
        assert_eq!(fingerprint_hamming(fp, fp), 0);
    }
}

#[test]
fn similarity_is_symmetric() {
    // sim(a, b) == sim(b, a) for any pair. Trivially true given that
    // both helpers (`fingerprint_hamming`, `fingerprint_distance`) use
    // commutative ops, but the contract is worth pinning.
    let a = fp_from([&random_pattern(11)[0], &random_pattern(11)[1], &random_pattern(11)[2]]);
    let b = fp_from([&random_pattern(22)[0], &random_pattern(22)[1], &random_pattern(22)[2]]);
    let sab = fingerprint_similarity_pct(&a, &b);
    let sba = fingerprint_similarity_pct(&b, &a);
    assert!((sab - sba).abs() < 1e-6, "asymmetry: {sab} vs {sba}");
    assert_eq!(fingerprint_hamming(&a, &b), fingerprint_hamming(&b, &a));
}

#[test]
fn identity_outranks_any_other_pairing() {
    // For every (target, neighbour) we generate, sim(target, target) >=
    // sim(target, neighbour). This is the "identity is maximum" axiom —
    // a perceptual hash that can't satisfy it isn't useful for ranking.
    let base = pattern_gradient();
    let neighbours = [
        fp_from([&pattern_blur(&base, 1)[0], &pattern_blur(&base, 1)[1], &pattern_blur(&base, 1)[2]]),
        fp_from([&pattern_shift(&base, 5)[0], &pattern_shift(&base, 5)[1], &pattern_shift(&base, 5)[2]]),
        fp_from([&pattern_desaturate(&base)[0], &pattern_desaturate(&base)[1], &pattern_desaturate(&base)[2]]),
        fp_from([&pattern_flip(&base)[0], &pattern_flip(&base)[1], &pattern_flip(&base)[2]]),
        fp_from([&random_pattern(99)[0], &random_pattern(99)[1], &random_pattern(99)[2]]),
    ];
    let target = fp_from([&base[0], &base[1], &base[2]]);
    let self_sim = fingerprint_similarity_pct(&target, &target);
    for fp in &neighbours {
        let s = fingerprint_similarity_pct(&target, fp);
        assert!(
            self_sim >= s,
            "self-sim ({self_sim}) should ≥ pair-sim ({s})"
        );
    }
}

#[test]
fn random_pairs_cluster_near_zero() {
    // With the re-anchored scale two unrelated 48-byte fingerprints
    // should land in the noise band. Statistical reality: with only 45
    // bits, a random pair has roughly a 0.05 % chance per draw of
    // crossing 50 % similarity by accident. Over 200 trials we expect
    // the *mean* to sit near 5–10 % (clamping pulls it down from the
    // pre-clamp ~50 % baseline) and a handful of outliers may reach
    // 50–65 %. The assertion budgets that.
    let n_pairs = 200;
    let mut total = 0.0f32;
    let mut max_seen = 0.0f32;
    let mut over_30 = 0;
    for seed in 0..n_pairs {
        let a_pat = random_pattern(seed * 2 + 1);
        let b_pat = random_pattern(seed * 2 + 2);
        let a = fp_from([&a_pat[0], &a_pat[1], &a_pat[2]]);
        let b = fp_from([&b_pat[0], &b_pat[1], &b_pat[2]]);
        let s = fingerprint_similarity_pct(&a, &b);
        total += s;
        if s > max_seen {
            max_seen = s;
        }
        if s > 30.0 {
            over_30 += 1;
        }
    }
    let mean = total / n_pairs as f32;
    assert!(
        mean < 15.0,
        "random pair mean similarity should sit in noise band, got {mean}"
    );
    // Outliers above 30 % must be rare — the bulk of unrelated pairs
    // collapses to 0 because they cross the half-bit threshold.
    assert!(
        over_30 < n_pairs / 10,
        "too many random pairs leaked above 30 %: {over_30}/{n_pairs}"
    );
    // No random pair should ever look like a real copy (≥ 80 %).
    assert!(
        max_seen < 80.0,
        "no random pair should look like a real copy, got {max_seen}"
    );
}

/// Stamp `n_flips` byte changes into a pattern at deterministic
/// positions. Each flip XORs the byte with a chosen mask so the dHash
/// bit at that boundary is more likely to flip too. Useful for "more
/// perturbed → lower similarity" assertions where we want a controlled
/// monotonic increase in Hamming distance rather than relying on a
/// transformation (blur, desat, …) whose effect on dHash isn't strictly
/// monotonic.
fn pattern_with_flips(base: &[[u8; FP_PER_CHANNEL]; 3], n_flips: usize) -> [[u8; FP_PER_CHANNEL]; 3] {
    let mut out = *base;
    let positions = [
        (0, 1),
        (1, 5),
        (2, 9),
        (0, 13),
        (1, 0),
        (2, 4),
        (0, 8),
        (1, 12),
        (2, 1),
        (0, 5),
        (1, 9),
        (2, 13),
    ];
    for (c, i) in positions.iter().take(n_flips) {
        // Big jump that crosses the comparison threshold against the
        // adjacent tile, guaranteeing the dHash bit at boundary i flips.
        out[*c][*i] = out[*c][*i].wrapping_sub(40);
    }
    out
}

#[test]
fn lighter_perturbation_outranks_heavier() {
    // Discrimination axiom: more perturbed → lower similarity. We use
    // deterministic byte flips instead of blur because heavy blur on a
    // small 16-tile gradient eventually flattens to a constant, at which
    // point dHash bits stop being meaningful (all comparisons tie). Byte
    // flips give us a clean monotonic increase in Hamming distance.
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);

    let mild = pattern_with_flips(&base, 2);
    let medium = pattern_with_flips(&base, 6);
    let heavy = pattern_with_flips(&base, 12);

    let s_mild = fingerprint_similarity_pct(&target, &fp_from([&mild[0], &mild[1], &mild[2]]));
    let s_medium = fingerprint_similarity_pct(&target, &fp_from([&medium[0], &medium[1], &medium[2]]));
    let s_heavy = fingerprint_similarity_pct(&target, &fp_from([&heavy[0], &heavy[1], &heavy[2]]));

    assert!(
        s_mild >= s_medium && s_medium >= s_heavy,
        "perturbation severity must lower similarity monotonically: \
         mild={s_mild}, medium={s_medium}, heavy={s_heavy}"
    );
    // The mildest perturbation must stay clearly above zero (it's only
    // a couple of bit flips).
    assert!(s_mild > 70.0, "2 flips should remain very similar, got {s_mild}");
}

#[test]
fn mild_smoothing_stays_close_to_original() {
    // A small-radius moving average preserves the gradient direction;
    // most dHash bits stay the same → similarity remains high (above
    // the noise floor by a wide margin). Heavier smoothing collapses
    // gradients to constants — out of scope of *this* test (see
    // `lighter_perturbation_outranks_heavier` for the strict ordering
    // assertion).
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);
    let mild = pattern_blur(&base, 1);
    let s = fingerprint_similarity_pct(&target, &fp_from([&mild[0], &mild[1], &mild[2]]));
    assert!(
        s > 40.0,
        "mild smoothing should keep similarity well above noise, got {s}"
    );
}

#[test]
fn flipped_copy_drops_sharply() {
    // A 180°-rotated copy ("flip every channel strip") was the
    // regression originally reported by the user: byte-magnitude L1 said
    // 97.9 % similar. With dHash flipping reverses adjacent comparisons
    // → many Hamming bits flip → similarity should land near the noise
    // floor (low end of 0..30).
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);
    let flipped = pattern_flip(&base);
    let fp_flipped = fp_from([&flipped[0], &flipped[1], &flipped[2]]);
    let s = fingerprint_similarity_pct(&target, &fp_flipped);
    assert!(
        s < 30.0,
        "180°-flipped copy must be far from original, got {s}"
    );
}

#[test]
fn desaturated_copy_loses_chroma_signal() {
    // Desaturation maps G and B to R — the two chroma strips collapse
    // to the same gradient. The R strip stays identical, but the G/B
    // dHash bits diverge wherever the original channels had distinct
    // gradients. Expect similarity to drop below the "near-copy" band
    // even though the R channel is unchanged.
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);
    let desat = pattern_desaturate(&base);
    let fp_desat = fp_from([&desat[0], &desat[1], &desat[2]]);
    let s = fingerprint_similarity_pct(&target, &fp_desat);
    assert!(s < 80.0, "desaturated copy should drop below 80 %, got {s}");
}

#[test]
fn ranking_returns_expected_top_3() {
    // Stage a target with five candidates of known relationship and
    // verify `rank_neighbors` returns them sorted by similarity. The
    // top entry must be the lightest perturbation; the bottom two must
    // be the flip and the random pattern.
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);

    let two_flips = pattern_with_flips(&base, 2);
    let many_flips = pattern_with_flips(&base, 10);
    let desat = pattern_desaturate(&base);
    let flipped = pattern_flip(&base);
    let random = random_pattern(42);

    let catalog: Vec<(String, Fingerprint)> = vec![
        (
            "flipped".into(),
            fp_from([&flipped[0], &flipped[1], &flipped[2]]),
        ),
        (
            "many_flips".into(),
            fp_from([&many_flips[0], &many_flips[1], &many_flips[2]]),
        ),
        (
            "two_flips".into(),
            fp_from([&two_flips[0], &two_flips[1], &two_flips[2]]),
        ),
        (
            "desat".into(),
            fp_from([&desat[0], &desat[1], &desat[2]]),
        ),
        (
            "random".into(),
            fp_from([&random[0], &random[1], &random[2]]),
        ),
        ("self".into(), target),
    ];
    let ranked = rank_neighbors(&target, &catalog, "self", 5);

    // The catalog must yield exactly the five non-self entries.
    assert_eq!(ranked.len(), 5);
    let names: Vec<&str> = ranked.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(!names.contains(&"self"), "self entry must be filtered out");

    // Top is the mildest perturbation (two byte flips on a gradient).
    assert_eq!(
        ranked[0].0, "two_flips",
        "two-flip variant should be ranked first; got {:?}",
        names
    );
    // The bottom two must come from {flipped, random} in some order —
    // those are the maximally divergent candidates.
    let bottom_names: std::collections::HashSet<&str> =
        ranked[3..].iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        bottom_names.contains("flipped") && bottom_names.contains("random"),
        "bottom two should be flipped + random, got {bottom_names:?}"
    );

    // Sorted descending.
    for w in ranked.windows(2) {
        assert!(
            w[0].2 >= w[1].2,
            "rank_neighbors must sort descending: {:.1} then {:.1}",
            w[0].2,
            w[1].2
        );
    }
}

#[test]
fn rank_returns_at_most_limit() {
    // The `limit` argument bounds output even when more candidates are
    // available. Catalogues with N entries and limit=k must return k.
    let base = pattern_gradient();
    let target = fp_from([&base[0], &base[1], &base[2]]);
    let mut catalog: Vec<(String, Fingerprint)> = Vec::new();
    for i in 0..10 {
        let p = random_pattern(i as u64 * 7 + 3);
        catalog.push((format!("rand_{i}"), fp_from([&p[0], &p[1], &p[2]])));
    }
    let r = rank_neighbors(&target, &catalog, "missing-self", 3);
    assert_eq!(r.len(), 3);
}

#[test]
fn dhash_bits_top_bits_unused() {
    // The 45-bit dHash is packed into the low half of a u64; bits 45+
    // must always be 0 — otherwise Hamming would over-count.
    let fp = fp_from([&pattern_gradient()[0], &pattern_gradient()[1], &pattern_gradient()[2]]);
    let bits = dhash_bits(&fp);
    let mask_above_45: u64 = !((1u64 << 45) - 1);
    assert_eq!(bits & mask_above_45, 0, "dHash leaked into bits ≥ 45");
}

#[test]
fn distance_is_zero_iff_identical() {
    // Raw L1 helper is still around for the JSON view; it has to be 0
    // exactly when fingerprints are equal, non-zero otherwise.
    let a = fp_from([&pattern_gradient()[0], &pattern_gradient()[1], &pattern_gradient()[2]]);
    let mut b = a;
    assert_eq!(fingerprint_distance(&a, &b), 0);
    b[5] = b[5].wrapping_add(7);
    assert!(fingerprint_distance(&a, &b) >= 7);
}

#[test]
fn opaque_kind_fingerprint_falls_back_to_cid() {
    // Opaque / text don't have meaningful perceptual content; the
    // fingerprint is deterministically derived from data_cid so that
    // identical PUTs of the same payload still compare as similar.
    let mut m = image_manifest(0);
    m.kind = ObjectKind::Opaque;
    for i in 0..32 {
        m.data_cid[i] = i as u8;
    }
    let fp1 = perceptual_fingerprint(&m, &[]);
    let fp2 = perceptual_fingerprint(&m, &[]);
    assert_eq!(fp1, fp2);
    // First 32 bytes mirror the CID; the trailing 16 mirror its prefix.
    for i in 0..32 {
        assert_eq!(fp1[i], i as u8);
    }
    for i in 0..16 {
        assert_eq!(fp1[32 + i], i as u8);
    }
}

// ===== Realistic scenario from the demo cluster ============================

/// Replays the exact mandala-vs-photo discrimination that motivated
/// re-anchoring. We construct a "photo"-like gradient and
/// a "mandala"-like roughly-uniform pattern with means in the 100-130
/// band; the old `(1 − h/45) · 100` formula gave both ≈ 50-70 %; the
/// re-anchored scale should now flag mandala as essentially uncorrelated.
#[test]
fn mandala_like_pattern_clamps_to_low_similarity() {
    let photo = pattern_gradient();
    let mut mandala = [[0u8; FP_PER_CHANNEL]; 3];
    for c in 0..3 {
        for i in 0..FP_PER_CHANNEL {
            // Mostly-uniform mid-gray with a tiny periodic ripple.
            mandala[c][i] = 110 + ((i as u8) % 3) * 4;
        }
    }

    let fp_photo = fp_from([&photo[0], &photo[1], &photo[2]]);
    let fp_mandala = fp_from([&mandala[0], &mandala[1], &mandala[2]]);
    let s = fingerprint_similarity_pct(&fp_photo, &fp_mandala);
    assert!(
        s < 30.0,
        "mandala-like pattern should not look like a real photo, got {s}"
    );
}
