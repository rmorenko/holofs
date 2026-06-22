//! Deterministic xorshift64 — reproducibility for demos and tests.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // `| 1` guards against the degenerate 0 state (xorshift sticks there).
        Rng(seed | 1)
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
