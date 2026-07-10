//! Galois field GF(2⁸) arithmetic.
//!
//! ## Field definition
//!
//! Elements are 8-bit unsigned integers; addition is XOR (`⊕`); multiplication
//! is polynomial multiplication modulo the irreducible polynomial
//!
//! ```text
//! p(x) = x⁸ + x⁴ + x³ + x² + 1   (=0x11d, the Rijndael / AES polynomial)
//! ```
//!
//! ## Implementation
//!
//! We precompute log / exp tables relative to the generator α = `0x02`. Then
//!
//! ```text
//! a · b = exp[log[a] + log[b]]    (for a,b ≠ 0)
//! a⁻¹  = exp[255 − log[a]]
//! ```
//!
//! The `exp` table is doubled to 512 entries so the addition `log[a] + log[b]`
//! never wraps and we avoid a modulo in the hot path.
//!
//! ## Reference
//!
//! - Lin & Costello, *Error Control Coding: Fundamentals and Applications*
//!   (2nd ed., 2004), §2.6 — Galois fields and finite-field arithmetic.

/// Pre-built log / exp tables for GF(2⁸).
///
/// Construct once via [`Gf::new`] and reuse: the tables are small (768 B total)
/// and `mul` / `inv` become two table lookups plus an addition. Clone-friendly.
pub struct Gf {
    /// Exponent table: `exp[i] = αⁱ mod p(x)`. Doubled (length 512) so that
    /// `exp[log[a] + log[b]]` avoids a modulo in `mul`.
    pub exp: [u8; 512],
    /// Discrete logarithm table: `log[αⁱ] = i`. `log[0]` is unused (treated by
    /// `mul` via an early-out).
    pub log: [u8; 256],
}

impl Gf {
    /// Build the log / exp tables. Costs ~512 iterations; do it once at startup.
    #[must_use]
    pub fn new() -> Self {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Gf { exp, log }
    }

    /// Multiplication in GF(2⁸).
    ///
    /// Constant-time only in the sense of "no branches over secret bits"
    /// **except** the `a == 0 || b == 0` early-out and the table lookups
    /// (which leak through cache timing). This is acceptable for holofs: the
    /// shard coefficients themselves are public — they ship in every shard.
    /// **Do not** reuse this for secret-key crypto.
    #[inline]
    #[must_use]
    pub fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    /// Precompute a 256-byte lookup table `t[b] = a · b` for a fixed
    /// scalar `a`. v3-10: RLNC encode multiplies each `ci` against
    /// every byte of one source chunk — for `symbol_len > 256` it's
    /// materially cheaper to build the table once and index it than to
    /// call `mul(ci, b)` per byte. Amortised cost per output byte
    /// drops from two lookups + one add to one lookup.
    #[must_use]
    pub fn mul_table(&self, a: u8) -> [u8; 256] {
        let mut t = [0u8; 256];
        if a == 0 {
            return t;
        }
        let log_a = self.log[a as usize] as usize;
        // t[0] stays 0 (0 · anything = 0). Everything else uses the
        // same log/exp lookup as `mul` but avoids re-reading `log[a]`.
        for (b_val, slot) in t.iter_mut().enumerate().skip(1) {
            *slot = self.exp[log_a + self.log[b_val] as usize];
        }
        t
    }

    /// Multiplicative inverse in GF(2⁸).
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `a == 0` — the field has no inverse
    /// for zero. Release builds silently return a garbage value (the
    /// pre-existing behaviour) rather than paying for the check on
    /// the hot RLNC decode loop.
    #[inline]
    #[must_use]
    pub fn inv(&self, a: u8) -> u8 {
        debug_assert!(a != 0, "Gf::inv(0) is undefined — caller must guard");
        self.exp[255 - self.log[a as usize] as usize]
    }
}

impl Default for Gf {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_zero_absorbs() {
        let gf = Gf::new();
        for x in 0u8..=255 {
            assert_eq!(gf.mul(0, x), 0);
            assert_eq!(gf.mul(x, 0), 0);
        }
    }

    #[test]
    fn mul_one_is_identity() {
        let gf = Gf::new();
        for x in 0u8..=255 {
            assert_eq!(gf.mul(1, x), x);
            assert_eq!(gf.mul(x, 1), x);
        }
    }

    #[test]
    fn mul_commutative() {
        let gf = Gf::new();
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                assert_eq!(gf.mul(a, b), gf.mul(b, a));
            }
        }
    }

    #[test]
    fn mul_associative_spot_check() {
        let gf = Gf::new();
        for a in [1u8, 2, 7, 31, 100, 200, 255] {
            for b in [1u8, 3, 17, 64, 128, 199] {
                for c in [1u8, 5, 41, 77, 250] {
                    assert_eq!(gf.mul(gf.mul(a, b), c), gf.mul(a, gf.mul(b, c)));
                }
            }
        }
    }

    #[test]
    fn mul_distributes_over_xor() {
        // a · (b ⊕ c) == (a·b) ⊕ (a·c). Linearity — without this RLNC breaks.
        let gf = Gf::new();
        for a in 0u8..=255 {
            for b in 0u8..=63 {
                for c in 0u8..=63 {
                    assert_eq!(gf.mul(a, b ^ c), gf.mul(a, b) ^ gf.mul(a, c));
                }
            }
        }
    }

    #[test]
    fn mul_by_inverse_is_one() {
        let gf = Gf::new();
        for a in 1u8..=255 {
            assert_eq!(gf.mul(a, gf.inv(a)), 1, "a = {a}");
        }
    }

    #[test]
    fn inv_is_involutive() {
        let gf = Gf::new();
        for a in 1u8..=255 {
            assert_eq!(gf.inv(gf.inv(a)), a, "a = {a}");
        }
    }
}
