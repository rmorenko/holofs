//! Deterministic xorshift64 — reproducibility for demos and tests.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // SplitMix64 finalizer to decorrelate adjacent seeds. The
        // previous `seed | 1` mapping identified `0` with `1` and
        // `2` with `3` etc. — consecutive `data_cid`-derived seeds
        // produced identical RLNC coefficient sequences, which is
        // a subtle determinism bug for callers who assume distinct
        // manifests get distinct encodings. The finalizer avalanches
        // every input bit; a trailing `.max(1)` still guards the
        // xorshift dead state.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(z.max(1))
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(0xC0FFEE);
        let mut b = Rng::new(0xC0FFEE);
        for _ in 0..1024 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let mut diffs = 0;
        for _ in 0..64 {
            if a.next() != b.next() {
                diffs += 1;
            }
        }
        assert!(
            diffs > 50,
            "different seeds must produce different sequences"
        );
    }

    #[test]
    fn seed_zero_does_not_collapse() {
        let mut r = Rng::new(0);
        let mut any_nonzero = false;
        for _ in 0..16 {
            if r.next() != 0 {
                any_nonzero = true;
            }
        }
        assert!(
            any_nonzero,
            "xorshift from 0-state is dead; new() must cure this"
        );
    }

    /// B14 regression: `seed | 1` merged `0`/`1` and `2`/`3` etc.,
    /// so two adjacent `data_cid`-derived seeds produced identical
    /// RLNC coefficient streams. SplitMix64 finalizer avalanches
    /// every input bit.
    #[test]
    fn adjacent_seeds_diverge() {
        for (a_seed, b_seed) in [(0u64, 1), (2, 3), (100, 101), (u64::MAX - 1, u64::MAX)] {
            let mut a = Rng::new(a_seed);
            let mut b = Rng::new(b_seed);
            let mut diffs = 0;
            for _ in 0..32 {
                if a.next() != b.next() {
                    diffs += 1;
                }
            }
            assert!(
                diffs > 28,
                "seeds {a_seed}/{b_seed} produced correlated streams ({diffs}/32 diffs)"
            );
        }
    }

    #[test]
    fn byte_covers_distribution() {
        // Sanity check: 16K bytes cover at least 200 distinct values.
        let mut r = Rng::new(0xABCDEF);
        let mut seen = [false; 256];
        for _ in 0..16_384 {
            seen[r.byte() as usize] = true;
        }
        let coverage = seen.iter().filter(|&&b| b).count();
        assert!(
            coverage >= 200,
            "expected ≥200 unique bytes, got {coverage}"
        );
    }
}
