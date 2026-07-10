//! Fuzz `Whitelist::decode`.
//!
//! Whitelist parses one length-prefixed entry list (B1 site) followed
//! by fixed-size pubkey + signature blocks. This target keeps looking
//! for any other reachable panic in the parser.
//!
//! Run under nightly:
//!
//! ```sh
//! cargo +nightly fuzz run whitelist_decode -- -max_len=65536
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = holofs_storage::whitelist::Whitelist::decode(data);
});
