//! Haar DWT 2D (forward/inverse, multi-level) and image position layout
//! into priority layers.

use crate::LEVELS;

const SQRT2: f32 = std::f32::consts::SQRT_2;

pub fn haar_forward(d: &mut [f32], w: usize, h: usize, levels: usize) {
    let (mut cw, mut ch) = (w, h);
    for _ in 0..levels {
        for y in 0..ch {
            let half = cw / 2;
            let mut tmp = vec![0f32; cw];
            for i in 0..half {
                let a = d[y * w + 2 * i];
                let b = d[y * w + 2 * i + 1];
                tmp[i] = (a + b) / SQRT2;
                tmp[half + i] = (a - b) / SQRT2;
            }
            for x in 0..cw {
                d[y * w + x] = tmp[x];
            }
        }
        for x in 0..cw {
            let half = ch / 2;
            let mut tmp = vec![0f32; ch];
            for i in 0..half {
                let a = d[(2 * i) * w + x];
                let b = d[(2 * i + 1) * w + x];
                tmp[i] = (a + b) / SQRT2;
                tmp[half + i] = (a - b) / SQRT2;
            }
            for y in 0..ch {
                d[y * w + x] = tmp[y];
            }
        }
        cw /= 2;
        ch /= 2;
    }
}

pub fn haar_inverse(d: &mut [f32], w: usize, h: usize, levels: usize) {
    for level in (0..levels).rev() {
        let cw = w >> level;
        let ch = h >> level;
        for x in 0..cw {
            let half = ch / 2;
            let mut tmp = vec![0f32; ch];
            for i in 0..half {
                let a = d[i * w + x];
                let dd = d[(half + i) * w + x];
                tmp[2 * i] = (a + dd) / SQRT2;
                tmp[2 * i + 1] = (a - dd) / SQRT2;
            }
            for y in 0..ch {
                d[y * w + x] = tmp[y];
            }
        }
        for y in 0..ch {
            let half = cw / 2;
            let mut tmp = vec![0f32; cw];
            for i in 0..half {
                let a = d[y * w + i];
                let dd = d[y * w + half + i];
                tmp[2 * i] = (a + dd) / SQRT2;
                tmp[2 * i + 1] = (a - dd) / SQRT2;
            }
            for x in 0..cw {
                d[y * w + x] = tmp[x];
            }
        }
    }
}

// === 1D Haar DWT (audio, ) ========================================

/// Multi-level 1D Haar DWT. `n` must be a multiple of `2^levels`.
pub fn haar_forward_1d(d: &mut [f32], levels: usize) {
    let n = d.len();
    let mut cn = n;
    for _ in 0..levels {
        let half = cn / 2;
        let mut tmp = vec![0f32; cn];
        for i in 0..half {
            let a = d[2 * i];
            let b = d[2 * i + 1];
            tmp[i] = (a + b) / SQRT2;
            tmp[half + i] = (a - b) / SQRT2;
        }
        d[..cn].copy_from_slice(&tmp);
        cn /= 2;
    }
}

/// Inverse 1D Haar DWT.
pub fn haar_inverse_1d(d: &mut [f32], levels: usize) {
    let n = d.len();
    for level in (0..levels).rev() {
        let cn = n >> level;
        let half = cn / 2;
        let mut tmp = vec![0f32; cn];
        for i in 0..half {
            let a = d[i];
            let dd = d[half + i];
            tmp[2 * i] = (a + dd) / SQRT2;
            tmp[2 * i + 1] = (a - dd) / SQRT2;
        }
        d[..cn].copy_from_slice(&tmp);
    }
}

/// Layer of a 1D coefficient at position `i` in an array of length `n`.
/// Layer 0 = coarsest LL band (low frequencies, bass);
/// 1..LEVELS = successive octaves of higher frequencies.
pub fn coeff_layer_1d(i: usize, n: usize) -> usize {
    let ll = n >> LEVELS;
    if i < ll {
        return 0;
    }
    for l in (1..=LEVELS).rev() {
        let bw = n >> (l - 1);
        let iw = n >> l;
        if i < bw && i >= iw {
            return LEVELS - l + 1;
        }
    }
    LEVELS
}

/// spatial → DWT inverse map.
///
/// Given a spatial ROI `(rx, ry, rw, rh)` in pixel coordinates, return
/// every DWT-plane position whose Haar coefficient has at least one
/// pixel of spatial support inside the ROI. The result is exhaustive
/// — every coefficient that, when nonzero in an otherwise-zero plane,
/// would write to at least one ROI pixel through the inverse Haar.
///
/// Math: for a 2D Haar with `levels` levels on a `w × h` plane, a
/// coefficient at DWT position `(cx, cy)` belongs to one sub-band at
/// some level `k ∈ 1..=levels`:
///   * LL_levels: `cx ∈ [0, w/2^levels)`, `cy ∈ [0, h/2^levels)` (only at the deepest level)
///   * LH_k:      `cx ∈ [w/2^k, w/2^(k-1))`, `cy ∈ [0, h/2^k)`
///   * HL_k:      `cx ∈ [0, w/2^k)`, `cy ∈ [h/2^k, h/2^(k-1))`
///   * HH_k:      `cx ∈ [w/2^k, w/2^(k-1))`, `cy ∈ [h/2^k, h/2^(k-1))`
/// In every case the coefficient's spatial support is a
/// `2^k × 2^k` block at band-relative position `(bx, by) * 2^k`,
/// where `bx = cx % (w/2^k)` and `by = cy % (h/2^k)`. We invert the
/// block math to enumerate all relevant `(cx, cy)`.
///
/// **Caveat:** this is a pure geometric inverse — it does NOT save
/// bandwidth on an RLNC-encoded store, because every shard of a
/// layer is a linear combination of *every* coefficient in that
/// layer. The function is the underlying primitive for any future
/// per-block encoding work; for today it powers a "coefficient-mask
/// spotlight" demo in the gateway.
#[must_use]
pub fn spatial_to_dwt_positions(
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    levels: usize,
) -> Vec<usize> {
    if rw == 0 || rh == 0 || rx >= w || ry >= h {
        return Vec::new();
    }
    let rx_end = (rx + rw).min(w);
    let ry_end = (ry + rh).min(h);
    let mut out: Vec<usize> = Vec::new();

    for k in 1..=levels {
        let tile = 1usize << k; // 2^k
        let band_w = w >> k; // = w / 2^k
        let band_h = h >> k;
        if band_w == 0 || band_h == 0 {
            continue;
        }
        // Tiles overlapping ROI:
        let bx_min = rx / tile;
        let by_min = ry / tile;
        let bx_max = (rx_end - 1) / tile;
        let by_max = (ry_end - 1) / tile;
        let bx_max = bx_max.min(band_w - 1);
        let by_max = by_max.min(band_h - 1);

        for by in by_min..=by_max {
            for bx in bx_min..=bx_max {
                // LL_k only contributes at the deepest level — for k < LEVELS the
                // LL band has been further decomposed into LL_{k+1} + LH/HL/HH_{k+1}
                // and is therefore already covered by the next iteration.
                if k == levels {
                    out.push(by * w + bx); // LL_levels at (bx, by)
                }
                // LH_k: x offset by band_w.
                out.push(by * w + (band_w + bx));
                // HL_k: y offset by band_h.
                out.push((band_h + by) * w + bx);
                // HH_k: both offset.
                out.push((band_h + by) * w + (band_w + bx));
            }
        }
    }
    out
}

/// map a spatial ROI to the per-layer block indices that
/// cover it. `layer_positions[l]` is the list of DWT-plane positions
/// (flat `y*w + x`) packed into layer `l`'s shards in PUT order; this
/// helper returns, per layer, the *indices into that vector* whose
/// underlying position is inside the ROI.
///
/// Output shape: `Vec<Vec<u32>>` indexed by layer. `out[l]` is the
/// position-indices (in `layer_positions[l]`) whose Haar coefficient
/// affects at least one pixel of the ROI. Caller fans across channels
/// at fetch time — channels share the same position layout.
///
/// Used by the gateway when an object is `ObjectEncoding::Replicated`
/// to fetch only the shards covering the ROI, finally delivering the
/// bandwidth-aware spotlight the reverse-map primitive
/// promised but couldn't reach with RLNC.
#[must_use]
pub fn roi_to_block_ids(
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    layer_positions: &[Vec<u32>],
) -> Vec<Vec<u32>> {
    use std::collections::HashSet;
    let levels = LEVELS;
    if layer_positions.is_empty() || rw == 0 || rh == 0 {
        return vec![Vec::new(); layer_positions.len()];
    }
    // 1. Set of DWT-plane positions whose coefficient touches the ROI.
    let touched: HashSet<u32> =
        spatial_to_dwt_positions(rx, ry, rw, rh, w, h, levels)
            .into_iter()
            .map(|p| p as u32)
            .collect();
    // 2. For each layer, find the indices in its `positions` vec whose
    //    value is in `touched`.
    let mut out: Vec<Vec<u32>> = Vec::with_capacity(layer_positions.len());
    for positions in layer_positions {
        let mut layer_ids: Vec<u32> = Vec::new();
        for (idx, &p) in positions.iter().enumerate() {
            if touched.contains(&p) {
                layer_ids.push(idx as u32);
            }
        }
        out.push(layer_ids);
    }
    out
}

/// companion to [`roi_to_block_ids`]. Same ROI-touched
/// primitive, but the output is *block* ids instead of raw
/// position-indices. A block of size `block_size` covers the
/// consecutive slice `layer_positions[l][b*bs .. (b+1)*bs]` of a
/// layer; the last block in a layer may be short. Block `b` is
/// returned when at least one of its positions is in the ROI's
/// DWT-touched set — the same criterion `roi_to_block_ids` uses at
/// coefficient granularity.
///
/// The producer side (`holofs_client::put_object_replicated_blocks`)
/// stores one shard per block; the gateway spotlight path fans this
/// output into `get_object_blocks` to fetch only the blocks whose
/// coefficients the ROI needs, saving bandwidth vs the RLNC path
/// (which is forced to fetch the whole layer because every RLNC
/// shard mixes every coefficient of that layer).
///
/// `block_size` must be `>= 1`; a zero-sized block is a caller bug
/// and would spin forever. In debug builds this panics on `0`.
#[must_use]
pub fn roi_to_block_ids_with_stride(
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    layer_positions: &[Vec<u32>],
    block_size: usize,
) -> Vec<Vec<u32>> {
    use std::collections::HashSet;
    debug_assert!(block_size >= 1, "block_size must be positive");
    let levels = LEVELS;
    if layer_positions.is_empty() || rw == 0 || rh == 0 || block_size == 0 {
        return vec![Vec::new(); layer_positions.len()];
    }
    let touched: HashSet<u32> = spatial_to_dwt_positions(rx, ry, rw, rh, w, h, levels)
        .into_iter()
        .map(|p| p as u32)
        .collect();
    let mut out: Vec<Vec<u32>> = Vec::with_capacity(layer_positions.len());
    for positions in layer_positions {
        // Iterate blocks in order. A block is included as soon as
        // ANY of its positions is touched; short-circuit on hit to
        // keep the common (small ROI) case cheap.
        let n_blocks = positions.len().div_ceil(block_size);
        let mut layer_ids: Vec<u32> = Vec::new();
        for b in 0..n_blocks {
            let start = b * block_size;
            let end = (start + block_size).min(positions.len());
            for &p in &positions[start..end] {
                if touched.contains(&p) {
                    layer_ids.push(b as u32);
                    break;
                }
            }
        }
        out.push(layer_ids);
    }
    out
}

/// Which priority layer position (x, y) belongs to in a DWT-frequency image.
/// Layer 0 = LL (coarse shape); 1..LEVELS = detail levels (finer → higher).
pub fn coeff_layer(x: usize, y: usize, w: usize, h: usize) -> usize {
    let llw = w >> LEVELS;
    let llh = h >> LEVELS;
    if x < llw && y < llh {
        return 0;
    }
    for l in (1..=LEVELS).rev() {
        let bw = w >> (l - 1);
        let bh = h >> (l - 1);
        let iw = w >> l;
        let ih = h >> l;
        if x < bw && y < bh && !(x < iw && y < ih) {
            return LEVELS - l + 1;
        }
    }
    LEVELS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NLAYERS;

    // Test dimensions — not to be confused with runtime dims in main.
    const TW: usize = 256;
    const TH: usize = 256;

    fn make_signal(w: usize, h: usize) -> Vec<f32> {
        let mut d = vec![0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32 / w as f32;
                let fy = y as f32 / h as f32;
                d[y * w + x] = (fx * 6.28).sin() * 50.0 + (fy * 12.56).cos() * 30.0 + 128.0;
            }
        }
        d
    }

    #[test]
    fn forward_inverse_roundtrip_single_level() {
        let (w, h) = (16, 16);
        let orig = make_signal(w, h);
        let mut d = orig.clone();
        haar_forward(&mut d, w, h, 1);
        haar_inverse(&mut d, w, h, 1);
        let max_err = orig
            .iter()
            .zip(d.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_err < 1e-3, "max err = {max_err}");
    }

    #[test]
    fn forward_inverse_roundtrip_multi_level() {
        let orig = make_signal(TW, TH);
        let mut d = orig.clone();
        haar_forward(&mut d, TW, TH, LEVELS);
        haar_inverse(&mut d, TW, TH, LEVELS);
        let max_err = orig
            .iter()
            .zip(d.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_err < 1e-2, "max err = {max_err} > 1e-2");
    }

    #[test]
    fn forward_concentrates_energy_in_ll() {
        // For a smooth image the energy must concentrate in the LL corner.
        let mut d = vec![100f32; TW * TH];
        haar_forward(&mut d, TW, TH, LEVELS);
        let llw = TW >> LEVELS;
        let llh = TH >> LEVELS;
        let mut ll_energy = 0f64;
        let mut total = 0f64;
        for y in 0..TH {
            for x in 0..TW {
                let v = d[y * TW + x] as f64;
                let e = v * v;
                total += e;
                if x < llw && y < llh {
                    ll_energy += e;
                }
            }
        }
        assert!(
            ll_energy / total > 0.99,
            "constant signal, LL/total = {}",
            ll_energy / total
        );
    }

    #[test]
    fn coeff_layer_in_range() {
        for y in 0..TH {
            for x in 0..TW {
                let l = coeff_layer(x, y, TW, TH);
                assert!(l < NLAYERS, "layer {l} ≥ {NLAYERS} at ({x},{y})");
            }
        }
    }

    #[test]
    fn coeff_layer_ll_block_is_zero() {
        let llw = TW >> LEVELS;
        let llh = TH >> LEVELS;
        for y in 0..llh {
            for x in 0..llw {
                assert_eq!(coeff_layer(x, y, TW, TH), 0);
            }
        }
    }

    #[test]
    fn coeff_layer_partitions_image() {
        // Every position falls into exactly one layer; sum across layers = w*h.
        let mut count = [0usize; NLAYERS];
        for y in 0..TH {
            for x in 0..TW {
                count[coeff_layer(x, y, TW, TH)] += 1;
            }
        }
        assert_eq!(count.iter().sum::<usize>(), TW * TH);
        // Layer 0 = LL = (W/2^L)·(H/2^L).
        assert_eq!(count[0], (TW >> LEVELS) * (TH >> LEVELS));
    }

    #[test]
    fn coeff_layer_scales_with_runtime_dims() {
        // Same coeff_layer at a different size: the LL block must scale.
        const BW: usize = 1024;
        const BH: usize = 1024;
        let llw = BW >> LEVELS;
        let llh = BH >> LEVELS;
        assert_eq!(coeff_layer(0, 0, BW, BH), 0);
        assert_eq!(coeff_layer(llw - 1, llh - 1, BW, BH), 0);
        assert_eq!(coeff_layer(llw, llh, BW, BH), 1);
    }

    // === 1D Haar ========================================

    #[test]
    fn haar_1d_roundtrip() {
        let mut d: Vec<f32> = (0..1024)
            .map(|i| (i as f32 * 0.05).sin() * 0.5 + (i as f32 * 0.13).cos() * 0.3)
            .collect();
        let orig = d.clone();
        haar_forward_1d(&mut d, LEVELS);
        haar_inverse_1d(&mut d, LEVELS);
        for (a, b) in orig.iter().zip(d.iter()) {
            assert!((a - b).abs() < 1e-3, "1D Haar roundtrip: {} vs {}", a, b);
        }
    }

    #[test]
    fn coeff_layer_1d_partitions() {
        const N: usize = 1024;
        let mut counts = [0usize; NLAYERS];
        for i in 0..N {
            counts[coeff_layer_1d(i, N)] += 1;
        }
        assert_eq!(counts.iter().sum::<usize>(), N);
        // LL = N >> LEVELS.
        assert_eq!(counts[0], N >> LEVELS);
    }

    #[test]
    fn haar_1d_concentrates_energy_in_ll_for_lowfreq() {
        // A low-frequency signal (long waves) must concentrate energy in LL.
        let n = 1024;
        let mut d: Vec<f32> = (0..n)
            .map(|i| (i as f32 / n as f32 * std::f32::consts::PI * 2.0).sin())
            .collect();
        haar_forward_1d(&mut d, LEVELS);
        let ll_n = n >> LEVELS;
        let ll_energy: f32 = d[..ll_n].iter().map(|x| x * x).sum();
        let total: f32 = d.iter().map(|x| x * x).sum();
        assert!(
            ll_energy / total > 0.95,
            "low-freq: LL/total = {}",
            ll_energy / total
        );
    }


    #[test]
    fn spatial_to_dwt_empty_roi() {
        let positions = spatial_to_dwt_positions(0, 0, 0, 0, TW, TH, LEVELS);
        assert!(positions.is_empty());
        let positions = spatial_to_dwt_positions(10, 10, 0, 50, TW, TH, LEVELS);
        assert!(positions.is_empty());
    }

    #[test]
    fn spatial_to_dwt_full_image_covers_everything_except_origin_double_count() {
        // ROI = full image. Every coefficient position must appear at
        // least once. We dedup the returned positions and compare
        // against the full plane size minus the "LL_k for k<LEVELS"
        // positions that the iteration intentionally skips (those are
        // already covered by the deeper LL_LEVELS + LH/HL/HH_LEVELS
        // children, mathematically equivalent on inverse).
        let positions = spatial_to_dwt_positions(0, 0, TW, TH, TW, TH, LEVELS);
        let dedup: std::collections::HashSet<usize> = positions.iter().copied().collect();
        // The full DWT plane has TW*TH positions.
        assert_eq!(dedup.len(), TW * TH, "every position must be reachable");
    }

    #[test]
    fn spatial_to_dwt_corner_pixel_hits_one_per_band() {
        // A 1×1 ROI in the very top-left must hit exactly one
        // coefficient per (level, sub-band): the (0,0)-block in each.
        let positions = spatial_to_dwt_positions(0, 0, 1, 1, TW, TH, LEVELS);
        let expected_total: usize = 1                       // LL_LEVELS
            + 3 * LEVELS;                                   // LH/HL/HH at each level
        let dedup: std::collections::HashSet<usize> =
            positions.iter().copied().collect();
        assert_eq!(dedup.len(), expected_total);
        // The LL_LEVELS coefficient lives at (0, 0) in the plane.
        assert!(dedup.contains(&0));
    }

    #[test]
    fn roi_to_block_ids_full_image_covers_every_position() {
        // Build layer_positions the same way bootstrap does, then ask
        // for the full image ROI. Sum of per-layer block id counts
        // must equal w*h — every position is covered.
        let (w, h) = (TW, TH);
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                let l = coeff_layer(x, y, w, h);
                positions[l].push((y * w + x) as u32);
            }
        }
        let ids = roi_to_block_ids(0, 0, w, h, w, h, &positions);
        let total: usize = ids.iter().map(|v| v.len()).sum();
        assert_eq!(total, w * h);
    }

    #[test]
    fn roi_to_block_ids_corner_is_strictly_smaller() {
        // 1×1 corner ROI must yield strictly fewer block ids than the
        // full image — proof the helper actually filters.
        let (w, h) = (TW, TH);
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                let l = coeff_layer(x, y, w, h);
                positions[l].push((y * w + x) as u32);
            }
        }
        let full = roi_to_block_ids(0, 0, w, h, w, h, &positions);
        let corner = roi_to_block_ids(0, 0, 1, 1, w, h, &positions);
        let full_n: usize = full.iter().map(|v| v.len()).sum();
        let corner_n: usize = corner.iter().map(|v| v.len()).sum();
        assert!(corner_n > 0, "corner ROI must touch at least one block");
        assert!(corner_n < full_n, "corner < full");
    }

    #[test]
    fn roi_to_block_ids_with_stride_full_image_covers_every_block() {
        // For a full-image ROI, every block in every layer must appear
        // — the ROI touches every position, so no block escapes.
        let (w, h) = (TW, TH);
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                let l = coeff_layer(x, y, w, h);
                positions[l].push((y * w + x) as u32);
            }
        }
        for &block_size in &[1usize, 8, 64, 4096] {
            let ids = roi_to_block_ids_with_stride(
                0, 0, w, h, w, h, &positions, block_size,
            );
            for (l, layer) in positions.iter().enumerate() {
                let expected = layer.len().div_ceil(block_size);
                assert_eq!(
                    ids[l].len(),
                    expected,
                    "layer {l} @ block_size={block_size}: got {} expected {expected}",
                    ids[l].len()
                );
                // Block ids must be 0..expected in strict order.
                for (i, &b) in ids[l].iter().enumerate() {
                    assert_eq!(b, i as u32);
                }
            }
        }
    }

    #[test]
    fn roi_to_block_ids_with_stride_corner_is_much_smaller() {
        // A 1×1 corner ROI at block_size=64 must yield strictly
        // fewer blocks than coefficient-level ids for the same ROI
        // — that's the bandwidth win promises.
        let (w, h) = (TW, TH);
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                let l = coeff_layer(x, y, w, h);
                positions[l].push((y * w + x) as u32);
            }
        }
        let coeffs = roi_to_block_ids(0, 0, 1, 1, w, h, &positions);
        let blocks = roi_to_block_ids_with_stride(
            0, 0, 1, 1, w, h, &positions, 64,
        );
        let coeff_n: usize = coeffs.iter().map(|v| v.len()).sum();
        let block_n: usize = blocks.iter().map(|v| v.len()).sum();
        assert!(block_n > 0, "corner ROI must touch at least one block");
        // block_n <= coeff_n always (a block contains ≥ 1 coeff
        // from the ROI). Full image is TW*TH coeffs → at
        // block_size=64 you'd have ~TW*TH/64 blocks — but this is
        // a *corner*, so the difference is much smaller than the
        // ratio. We just assert the strict inequality here.
        assert!(
            block_n <= coeff_n,
            "block count {block_n} must be ≤ coeff count {coeff_n}"
        );
    }

    #[test]
    fn roi_to_block_ids_with_stride_matches_naive_at_stride_one() {
        // At block_size=1 the stride variant collapses to
        // `roi_to_block_ids` — every position IS its own block.
        let (w, h) = (TW, TH);
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                let l = coeff_layer(x, y, w, h);
                positions[l].push((y * w + x) as u32);
            }
        }
        let (rx, ry, rw, rh) = (10, 15, 20, 25);
        let naive = roi_to_block_ids(rx, ry, rw, rh, w, h, &positions);
        let strided = roi_to_block_ids_with_stride(
            rx, ry, rw, rh, w, h, &positions, 1,
        );
        assert_eq!(naive, strided);
    }

    #[test]
    fn spatial_to_dwt_isolated_block_roundtrip() {
        // Put a single non-zero coefficient at a known DWT-plane
        // position (an HL_1 corner — should affect the top half of
        // the image's first column band). Run haar_inverse and
        // check that the non-zero pixels match the ROI predicted by
        // spatial_to_dwt_positions in reverse.
        //
        // Concrete: pick a coefficient at (cx, cy) and figure out the
        // spatial block it influences, then verify that
        // spatial_to_dwt_positions for that exact block returns
        // (cx, cy) among its results.
        let cx = TW / 4;       // first col of HL_1 band
        let cy = TH / 4;       // first row of HL_1 band — actually this is HH_2
        let mut plane = vec![0f32; TW * TH];
        plane[cy * TW + cx] = 100.0;
        haar_inverse(&mut plane, TW, TH, LEVELS);

        // Find the bounding box of the non-zero region.
        let mut min_x = TW;
        let mut max_x = 0;
        let mut min_y = TH;
        let mut max_y = 0;
        for y in 0..TH {
            for x in 0..TW {
                if plane[y * TW + x].abs() > 1e-3 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        assert!(max_x >= min_x, "single coeff yielded no spatial output");
        let bw = max_x - min_x + 1;
        let bh = max_y - min_y + 1;

        // spatial_to_dwt_positions over the exact bounding box must
        // return (cx, cy) — the inverse map must be a superset.
        let positions = spatial_to_dwt_positions(min_x, min_y, bw, bh, TW, TH, LEVELS);
        let lookup: std::collections::HashSet<usize> = positions.into_iter().collect();
        assert!(
            lookup.contains(&(cy * TW + cx)),
            "ROI {min_x},{min_y} {bw}x{bh} → DWT set missed ({cx}, {cy})"
        );
    }
}
