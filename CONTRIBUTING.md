# Contributing to holofs

Thanks for considering a contribution! holofs is a young project — the bar for
changes is "would this pass a code review from someone who cares about the
holographic-degradation property?".

## Quick start

```sh
git clone https://github.com/holofs/holofs
cd holofs
cargo build --workspace --release
cargo test --workspace
cargo leptos build --project holofs-web --release  # builds SSR binary + WASM bundle
./target/release/holofs-web                         # http://127.0.0.1:8787/
```

MSRV is **Rust 1.75**. CI verifies this; please don't bump it casually.

## Workspace layout

```
crates/
  holofs-core/      math (GF, DWT, RLNC, Merkle) — no external deps
  holofs-model/     data types (Manifest, Directory, Placement)
  holofs-wire/      binary protocol over tokio TCP
  holofs-codec/     image / audio / text codecs
  holofs-storage/   node Store + Ed25519 identity + signed whitelist
  holofs-client/    PUT/GET/REPAIR/AUDIT + per-kind paths
  holofs-cluster/   health monitor, PoR auditor, reputation, rebalance
  holofs-analytics/ perceptual hash, MinHash, chunk diff, escrow
  holofs-gateway/   shared Gateway state + data-plane methods (no HTTP)
  holofs-cli/       binaries (holofs-node, holofs-admin, holofs-bench, ...)
  holofs-web/       axum + Leptos SSR frontend (binary: holofs-web)
```

Keep the dependency arrow pointing **down**: never make `holofs-core` depend on
anything else; never let `holofs-model` depend on `holofs-storage`, etc.

## Style

- Run `cargo fmt --all` before committing.
- Pass `cargo clippy --workspace --all-targets -- -D warnings`.
- Document every `pub` item with `///` (the `missing_docs` lint enforces this).
- All comments and doc-strings are in **English**.
- Unsafe code is **forbidden** at the workspace level
  (`#![deny(unsafe_code)]`).

## Testing

- Unit tests live alongside the code: `#[cfg(test)] mod tests { ... }`.
- Integration tests using a real TCP loopback cluster live in
  `crates/holofs-cli/tests/`.
- New features need tests. New bugfixes need a regression test.

## PR checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo doc --no-deps --workspace` builds with no warnings
- [ ] Public items have rustdoc
- [ ] CHANGELOG.md updated under `[Unreleased]`
- [ ] No new external dependencies without discussion in the PR description

## Releasing (maintainers)

1. Update `[workspace.package].version` in `Cargo.toml`.
2. Move `[Unreleased]` to a new `[X.Y.Z] - YYYY-MM-DD` section in CHANGELOG.md.
3. Commit, tag `vX.Y.Z`, push. CI builds release binaries and Docker image.
