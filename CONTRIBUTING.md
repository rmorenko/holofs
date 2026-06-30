# Contributing to holofs

Thanks for considering a contribution! holofs is a young project — the bar for
changes is "would this pass a code review from someone who cares about the
holographic-degradation property?".

## Quick start

```sh
git clone https://github.com/holofs/holofs
cd holofs
cargo build --workspace --release
cargo test --workspace --exclude holofs-e2e            # 320 unit + integration tests
cargo leptos build --release                           # builds SSR binary + WASM bundle
./target/release/holofs-web                            # http://127.0.0.1:8787/
```

MSRV is **Rust 1.81**. CI verifies this; please don't bump it casually.

## Workspace layout

```
crates/
  holofs-core/      math (GF, DWT, RLNC, Merkle, repair, rng, transform) — no external deps
  holofs-model/     data types (Manifest, Directory, Placement, NoLiveNodes)
  holofs-wire/      binary protocol over tokio TCP (length-prefixed frames)
  holofs-codec/     image / audio / text / opaque codecs
  holofs-storage/   per-node Store + Ed25519 identity + signed whitelist + TLS scaffold
  holofs-client/    PUT/GET/REPAIR/AUDIT + per-addr keepalive pool + RPC timeouts
  holofs-cluster/   health monitor, PoR auditor, reputation, rebalance, zone-aware placement
  holofs-embed/     CLIP-multilingual embeddings + HNSW ANN index for /search
  holofs-analytics/ perceptual hash, MinHash, chunk diff, escrow
  holofs-gateway/   catalog, decode pipeline, auto-repair-on-read, background scrub
  holofs-mcp/       Streamable-HTTP Model Context Protocol server
  holofs-web/       axum + Leptos 0.7 SSR frontend (binary: holofs-web)
  holofs-cli/       binaries (holofs-admin, holofs-bench, holofs-inspect, ...)
  holofs-e2e/       browser-driven (thirtyfour + chromedriver) end-to-end test harness
```

Keep the dependency arrow pointing **down**: never make `holofs-core` depend on
anything else; never let `holofs-model` depend on `holofs-storage`, etc. A full
crate dependency graph lives in
[`docs/architecture.md`](./docs/architecture.md#1-crate-dependency-graph).

## Style

- Run `cargo fmt --all` before committing.
- Pass `cargo clippy --workspace --all-targets -- -D warnings`.
- Document every `pub` item with `///` (the `missing_docs` lint enforces this).
- All comments and doc-strings are in **English**.
- Unsafe code is **forbidden** at the workspace level
  (`#![forbid(unsafe_code)]`). If you need a syscall, shell out via
  `std::process::Command` — see `holofs-e2e/src/lib.rs::GatewayProcess::
  kill_and_wait` for the SIGTERM pattern.

## Testing

Three layers:

- **Unit tests** live alongside the code (`#[cfg(test)] mod tests { ... }`).
  Use the mock-node TcpListener helper pattern from
  `crates/holofs-cluster/src/audit.rs::tests` or
  `crates/holofs-client/src/client.rs::rpc_tests` when you need
  network-touching tests without spawning a full cluster.
- **Integration tests** are per-crate under `tests/`. Existing examples:
  `holofs-cli/tests/distributed.rs` (real TCP loopback cluster) and
  `holofs-client/tests/tls_roundtrip.rs` (rustls test pair).
- **End-to-end tests** under `crates/holofs-e2e/tests/` drive a real Chrome
  via thirtyfour against a fresh `holofs-web` per scenario. Prerequisites:

  ```sh
  brew install --cask chromedriver    # or apt install chromium-chromedriver
  cargo leptos build --release        # so the e2e harness can spawn it
  cargo test -p holofs-e2e -- --test-threads=1
  ```

  10 tests are `#[ignore]`'d behind `--include-ignored` because they
  download the ~155 MiB DistilBERT-multilingual CLIP weights on first
  run. Pre-warm `~/.cache/huggingface/hub/` in CI runner images.

### Coverage

Run [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) to
verify your change doesn't regress the line-coverage on the measurable
surface (currently 91 %):

```sh
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude holofs-e2e --summary-only \
  --ignore-filename-regex \
  'tests/|holofs-e2e/|holofs-web/|holofs-cli/src/bin/|holofs-gateway/src/http_gateway\.rs|holofs-mcp/src/lib\.rs|holofs-embed/src/(model|text)\.rs|holofs-codec/src/image_io\.rs'
```

The exclusion regex hides code only reachable via the spawned gateway
(HTTP handlers, leptos SSR, CLIP model loader, CLI bins). That code IS
exercised by the e2e suite — it just doesn't show up in unit-level
coverage. End-to-end coverage requires SIGTERM-aware profraw dumping
in the gateway main loop (not shipped yet); see the §"Boundaries" note
in [`README.md`](./README.md#tests).

New features need tests. New bugfixes need a regression test —
preferably at the level closest to the bug:

- pure-data fix → unit test in the owning crate
- wire-protocol regression → mock-node test under `holofs-wire` /
  `holofs-client` / `holofs-cluster`
- HTTP-handler / UI bug → e2e test under `holofs-e2e/tests/`

## PR checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace --exclude holofs-e2e` passes (320 tests)
- [ ] `cargo test -p holofs-e2e -- --test-threads=1` passes when the
       change touches HTTP / UI surfaces (106 tests + 10 `#[ignore]`'d)
- [ ] `cargo doc --no-deps --workspace` builds with no warnings
- [ ] Public items have rustdoc
- [ ] CHANGELOG.md updated under `[Unreleased]`
- [ ] No new external dependencies without discussion in the PR description
- [ ] Coverage on the affected file did not drop (run `cargo llvm-cov`
       with the regex above before/after if in doubt)

## Releasing (maintainers)

1. Update `[workspace.package].version` in `Cargo.toml`.
2. Move `[Unreleased]` to a new `[X.Y.Z] - YYYY-MM-DD` section in CHANGELOG.md.
3. Run the full test sweep + coverage one more time on the release commit.
4. Commit, tag `vX.Y.Z`, push. CI builds release binaries and Docker image.
5. Refresh the localised `docs/{ru,de,fr,es}/*.md` translations if the
   release crosses a major feature boundary. Until they're refreshed,
   the stale-banner stays at the top of each file.
