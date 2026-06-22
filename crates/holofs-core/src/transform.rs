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

// === 1D Haar DWT (audio, Stage 9) ========================================

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

    // === 1D Haar (Stage 9, audio) ========================================

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
}
