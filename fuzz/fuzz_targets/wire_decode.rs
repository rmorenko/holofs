//! Fuzz the wire protocol's `Request::decode` and `Response::decode`.
//!
//! Companion to the review v2 P3.4 ask + the negative unit tests
//! added under B1: a random byte slice must never panic the decoder.
//! B1 fixed the specific `u32::MAX` OOM class; this target keeps
//! looking for whatever else the mutation engine finds — bad
//! discriminants, truncated variable-length regions, arithmetic
//! overflows in length prefixes, etc.
//!
//! Run under nightly:
//!
//! ```sh
//! cargo +nightly fuzz run wire_decode -- -max_len=65536
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = holofs_wire::Request::decode(data);
    let _ = holofs_wire::Response::decode(data);
});
