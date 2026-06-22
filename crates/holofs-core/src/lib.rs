//! # holofs-core
//!
//! Foundational primitives for the holofs holographic distributed filesystem.
//!
//! This crate is **dependency-free** (no external crates) and contains only
//! mathematics and constants. It is the bottom of the dependency stack — every
//! other holofs crate depends on it.
//!
//! ## Modules
//!
//! - [`gf`]        — Galois field GF(2⁸) arithmetic via log/exp tables.
//! - [`rng`]       — deterministic xorshift64 pseudo-random generator.
//! - [`transform`] — Haar Discrete Wavelet Transform (2D + 1D) and priority-layer mapping.
//! - [`rlnc`]      — Random Linear Network Coding over GF(2⁸): encode / decode
//!                   with systematic + RLNC shards, partial recovery, custom-K.
//! - [`cluster`]   — shard placement metadata (which node hosts which (channel, layer)).
//! - [`repair`]    — RLNC node regeneration (mix donors without full reconstruction).
//! - [`hash`]      — SHA-256 hand-rolled per FIPS 180-4. NIST-vector verified.
//! - [`merkle`]    — content addressing: data CID, shard hash, Merkle root.
//!
//! ## Object parameters (compile-time defaults)
//!
//! The constants below set the canonical encoding parameters for a v0 object.
//! Width / height are runtime via [`dims_from_env`]; everything else is
//! compile-time so generic code stays monomorphised.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod cluster;
pub mod gf;
pub mod hash;
pub mod merkle;
pub mod repair;
pub mod rlnc;
pub mod rng;
pub mod transform;

/// Default frame width (pixels) when `HOLOFS_W` env var is not set.
pub const DEFAULT_W: usize = 512;
/// Default frame height (pixels) when `HOLOFS_H` env var is not set.
pub const DEFAULT_H: usize = 512;
/// Number of Haar DWT decomposition levels for image / audio.
pub const LEVELS: usize = 3;
/// Number of priority layers (always `LEVELS + 1`: one LL band plus per-level details).
pub const NLAYERS: usize = LEVELS + 1;
/// RLNC threshold: minimum number of independent shards required to decode a layer.
pub const K: usize = 16;
/// Default cluster size for embedded demos.
pub const N_NODES: usize = 40;
/// Redundancy multiplier per priority layer.
///
/// `RED[0]` is the redundancy of the coarsest band (LL) — high redundancy so it
/// survives heavy node loss. `RED[NLAYERS-1]` is the finest band — low
/// redundancy because we accept losing fine detail first. This shape produces
/// the holographic degradation curve.
pub const RED: [f32; NLAYERS] = [4.0, 2.5, 1.6, 1.15];

/// Read frame dimensions from the `HOLOFS_W` / `HOLOFS_H` environment variables,
/// falling back to [`DEFAULT_W`] / [`DEFAULT_H`].
///
/// # Panics
///
/// Panics if W or H is not a positive multiple of `2^LEVELS` — the Haar DWT
/// pyramid requires this to fit cleanly.
#[must_use]
pub fn dims_from_env() -> (usize, usize) {
    let w: usize = std::env::var("HOLOFS_W")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_W);
    let h: usize = std::env::var("HOLOFS_H")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_H);
    let step = 1usize << LEVELS;
    assert!(
        w >= step && w % step == 0,
        "HOLOFS_W = {w} is not a multiple of 2^LEVELS = {step}"
    );
    assert!(
        h >= step && h % step == 0,
        "HOLOFS_H = {h} is not a multiple of 2^LEVELS = {step}"
    );
    (w, h)
}
