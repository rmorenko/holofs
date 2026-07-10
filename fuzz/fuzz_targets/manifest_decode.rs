//! Fuzz `Manifest::decode`.
//!
//! Same rationale as `wire_decode.rs` — the manifest binary format has
//! its own length-prefixed regions (n_per_layer, layer_positions,
//! nodes, shard_hashes, chunk_lens, text_minhash) that all had the
//! same `Vec::with_capacity` OOM shape until B1. A random byte
//! stream must never panic the decoder.
//!
//! Run under nightly:
//!
//! ```sh
//! cargo +nightly fuzz run manifest_decode -- -max_len=65536
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = holofs_model::manifest::Manifest::decode(data);
});
