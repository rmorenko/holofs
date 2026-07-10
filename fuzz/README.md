# holofs fuzz targets

Coverage-guided fuzzing for the parsers that sit on the untrusted
network / disk boundary. Compiles on nightly Rust with
[`cargo-fuzz`](https://rust-fuzz.github.io/book/) + `libfuzzer-sys`.

Excluded from the workspace so a normal `cargo build/test` doesn't
pull in `libfuzzer-sys` (which needs LLVM's fuzzer runtime and a
nightly toolchain).

## Targets

| Target             | Parser under test                            |
|--------------------|----------------------------------------------|
| `wire_decode`      | `holofs_wire::{Request, Response}::decode`   |
| `manifest_decode`  | `holofs_model::manifest::Manifest::decode`   |
| `whitelist_decode` | `holofs_storage::whitelist::Whitelist::decode` |

Each harness feeds a random byte slice into the decoder. Any panic —
`Vec::with_capacity` OOM, index out of range, arithmetic overflow —
is a crash the fuzzer records under `fuzz/artifacts/<target>/`.

The three targets track the review v1 B1 finding (bounded
`Vec::with_capacity` across all wire / manifest / whitelist length
prefixes). Regressions in the same class would surface here first.

## Running

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

# From the workspace root:
cargo +nightly fuzz run wire_decode      -- -max_len=65536
cargo +nightly fuzz run manifest_decode  -- -max_len=65536
cargo +nightly fuzz run whitelist_decode -- -max_len=65536
```

The `-max_len=65536` cap matches `holofs_wire::MAX_FRAME` (64 MB is
overkill for fuzzing throughput — 64 KB inputs cover every reachable
control-path branch and let the mutator run millions of trials per
hour).

Corpus is stored under `fuzz/corpus/<target>/`; artefacts (crashes,
timeouts, OOMs) land in `fuzz/artifacts/<target>/`. Neither directory
is committed — they grow unbounded and are per-machine anyway.

## When to run

- Before shipping any change that touches
  `holofs-wire::wire`, `holofs-model::manifest`, or
  `holofs-storage::whitelist`.
- Continuously in a nightly CI job for high-value branches
  (`develop`, release candidates).
- Whenever a new `Request` / `Response` variant is added — the fuzzer
  needs a few minutes to explore the new discriminants.

The property-sweep test `rlnc_roundtrip_property_sweep`
(`holofs-core::rlnc`) and the negative unit tests in
`holofs-wire::tests` cover the same class at unit-test speed and run
on every workspace `cargo test`. Fuzzing is the deeper follow-up.
