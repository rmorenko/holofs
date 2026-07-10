# Contributing to holofs

Thanks for considering a contribution! holofs is a young project — the bar for
changes is "would this pass a code review from someone who cares about the
holographic-degradation property?".

## Quick start

```sh
git clone https://github.com/holofs/holofs
cd holofs
cargo build --workspace --release
cargo test --workspace --exclude holofs-e2e            # workspace tests (~380)
cargo test -p holofs-web --features ssr --lib          # SSR-only lib tests (~70)
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
                    (18-module fan-out post v0.6.0; see docs/architecture.md § 1.1)
  holofs-testutils/ shared DisablePool + spawn_mock_node helpers (dev-only)
  holofs-mcp/       Streamable-HTTP Model Context Protocol server
  holofs-web/       axum + Leptos 0.7 SSR frontend (binary: holofs-web)
                    (21-module fan-out post v0.6.1; see docs/architecture.md § 1.3)
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

  A subset of tests are `#[ignore]`'d behind `--include-ignored`
  because they download the ~155 MiB DistilBERT-multilingual CLIP
  weights on first run — exact count drifts with the suite. Pre-warm
  `~/.cache/huggingface/hub/` in CI runner images.

### Coverage

Run [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) to
verify your change doesn't regress the line-coverage on the measurable
surface. The v0.5.0 baseline was 91 %; the v0.6.0 R1 refactor moved
a lot of code around without changing behaviour, so the number is
approximately preserved. Watch for regressions in the newly added
reliability modules (`supervised`, `timeout`, `backpressure`,
`admin_auth`):

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

## Reliability layer

Every long-running background task in `holofs-web` goes through
the `supervised` wrapper, and every `axum` route lives in one of
four buckets (short / medium / long / streaming) with its own
timeout + backpressure policy. Before touching any of the
following, please read the module-level docs and the sibling
unit tests:

| Concern | Module | See also |
|---|---|---|
| SIGTERM / SIGINT drain | `holofs_web::bootstrap` + `holofs_web::main` | `docs/operations.md § 5.6` |
| Panic-safe background loops | `holofs_web::supervised::supervised_spawn` | tests in the same module |
| Per-route timeouts | `holofs_web::timeout` | `docs/api.md § /metrics`, tests in the module |
| Per-bucket backpressure | `holofs_web::backpressure` | `docs/operations.md § 5.6` |
| Admin bearer-token auth | `holofs_web::admin_auth` | `docs/api.md § Admin auth` |
| Fail-loud catalog persist | `holofs_gateway::Gateway::persist_catalog` | tests in `holofs_gateway::http_gateway::tests` |
| Persistent reputation | `holofs_cluster::reputation` (`save_atomic` / `load_or_new`) | tests in the same module |
| Prometheus counters | `holofs_gateway::Gateway::observability_counters()` + `holofs_web::handlers::metrics` | `docs/operations.md § 6.1` |

New `Gateway` methods belong to their concern's sibling module
(`ingest`, `decode`, `search`, `versions`, ...) — see
`docs/architecture.md § 1.1` for the map. Nothing should grow
back into `http_gateway.rs`.

### Where new UI code and handlers go

`holofs-web` was similarly decomposed. Nothing new should grow
back into `lib.rs` or `handlers.rs` — both are now pure
module-registration front-doors.

| Concern | Module | Notes |
|---|---|---|
| New `#[server]` function | `holofs_web::server_fns` | Reads `Gateway` from Leptos context; hydrate stub is auto-generated. |
| New catalog / tree Leptos component | `holofs_web::catalog_ui` | Fifteen existing components + `MkdirForm` / `UploadForm` / `ObjectCard`. |
| New standalone page (`/foo`) | new sibling module (e.g. `holofs_web::foo`) | Mirror `holofs_web::health` / `holofs_web::similar` — add a `pub mod` to `lib.rs` and a `<Route path=path!("/foo") view=foo::Page/>` to `RoutedApp`. |
| New axum handler | pick a `handlers/*.rs` domain | `objects`, `dirops`, `uploads`, `versions`, `analytics`, `search`, `health`, `escrow`. Shared helpers → `handlers/util.rs`; new response projections → `handlers/response.rs`. |
| New pure helper (path validation, escape, header shortcut) | `handlers/util.rs` | pub(crate); handlers.rs itself owns no code — the top-level `pub use handlers::foo` is the only surface. |

See `docs/architecture.md § 1.3` for the full 21-module map and
the "rule of thumb" for extending each concern.

## PR checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace --exclude holofs-e2e` passes
- [ ] `cargo test -p holofs-web --features ssr --lib` passes when the
       change touches middleware / bootstrap in holofs-web
- [ ] `cargo test -p holofs-e2e -- --test-threads=1` passes when the
       change touches HTTP / UI surfaces (a subset is `#[ignore]`'d
       for reasons documented in `crates/holofs-e2e/README.md`)
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
