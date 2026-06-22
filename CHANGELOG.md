# Changelog

All notable changes to holofs are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Stage 11.5 — upload form polish.** Styled drop-zone (dashed border,
  hover/drag highlight), label-as-button replacing the native unstyled
  `<input type=file>`, monospace filename preview with size hint,
  matching text-input and submit button styles, drag-and-drop on the
  surrounding box via `assets/upload-init.js`. Works without JS — the
  label is a real click target, drag-drop is enhancement only.
- **Static `/assets/*` is now properly served.** `nest_service("/assets",
  ServeDir::new("target/site/assets"))` so the wildcard object route
  doesn't shadow it. `assets` joined the reserved top-segment list.
- **Stage 11.4 — file upload form on the catalog page.** Stage 9
  dropped the legacy `/admin/upload` form (it pointed at a route that
  hadn't existed since Phase 4) and never put a replacement back, so
  the UI had no way to add files — only `curl -X PUT` worked. New
  `POST /api/upload` multipart endpoint takes `parent`, optional
  `name`, and `file`; renames-on-the-way-in are honoured, otherwise
  the browser-supplied `filename` is used; the destination becomes
  `<parent>/<leaf>`. Wired into the catalog page as an `UploadForm`
  Leptos component placed above the existing `MkdirForm`. Body limit
  layered with the same 256 MiB cap as PUT / escrow.
- Catalog object cards now carry a `similar` link in the actions row.
  The `/similar/<name>` view existed since Stage 4 but had no entry
  point from the grid.

### Fixed
- **Stage 11.2 — `/inspect/<name>` no longer drops random shard thumbnails
  under concurrent render.** `Gateway::shard_payload` used to call
  `gather_layer` (a full cluster-wide collect) for every cell rendered
  on the page; with 444 cells firing in parallel from the browser the
  per-node fan-out saturated the cluster and `unwrap_or_default()`
  silently swallowed some responses, surfacing as spurious `404`s on
  ~8% of cells. The gateway now caches gathered shard vectors per
  `(name, channel, layer)` behind a `tokio::sync::OnceCell` so the first
  concurrent request does the gather and every other caller awaits the
  same future. Invalidated together with the decoded-object cache on
  every PUT / DELETE.
- **Stage 11.3 — body-size limit raised on every upload route.** axum's
  default `DefaultBodyLimit` (2 MiB) was rejecting realistic media
  uploads with the misleading error `multipart read: Error parsing
  multipart/form-data request` — the body got truncated mid-parse, not
  malformed. `PUT /<path>`, `POST /escrow/split`, and
  `POST /escrow/recover` are now layered with
  `DefaultBodyLimit::max(256 MiB)`. Verified end-to-end with 3 MB / 30
  MB multipart uploads and a 4 MB PUT.

### Added
- **Stage 11.1 — HTTP `Range` on `GET /<path>` and `GET /preview/<path>`.**
  RFC 9110 §14.2 byte-range support: single satisfiable range returns
  `206 Partial Content` with `Content-Range: bytes A-B/total`; suffix
  ranges (`bytes=-N`) and open-ended (`bytes=A-`) handled; unsatisfiable
  ranges yield `416` with `Content-Range: bytes */total`. Multi-range
  requests degrade to a `200` with the full body (no
  `multipart/byteranges` formatting). Object is decoded in full
  server-side; the response is a slice of the resulting buffer.

## [0.4.0] - 2026-06-22

### Added
- **Stage 10 — in-app docs viewer (`/help`).** New axum + Leptos route
  renders every file under `docs/` as themed HTML, with a sticky sidebar
  TOC, breadcrumb, and a language switcher. Mermaid diagrams and KaTeX
  math are rendered client-side: server-side `pulldown-cmark` rewrites
  ` ```mermaid ` fenced blocks to `<div class="mermaid">…</div>` and `$x$`
  / `$$x$$` math to KaTeX-friendly `\(…\)` / `\[…\]` wrappers. Mermaid
  and KaTeX assets load from jsDelivr only on `/help` routes; the catalog
  / health pages don't pay their weight.
- **Stage 10 — UI internationalisation (5 locales).** Static translation
  table (`crates/holofs-web/src/i18n.rs`) covering English (default),
  Russian, German, French, Spanish. Locale resolves from `?lang=<code>`
  via reactive context (`LocaleSignal`); the topbar carries a small
  switcher that rewrites the current URL with the new lang param without
  losing other query state.
- **Stage 10 — translated documentation.** Every `docs/*.md` file ships
  in four additional languages under `docs/{ru,de,fr,es}/`. `/help`
  serves the locale matching the request, falling back to English when a
  translated variant is missing.
- **Shared `Topbar` component (`ui::Topbar`).** Replaces nine inline
  `<header class="topbar">` copies across the pages — translations and
  the locale switcher are now wired in exactly one place.

### Changed
- Bumped workspace version `0.3.0 → 0.4.0` to reflect the new help /
  docs / i18n surface and the `pulldown-cmark` dependency addition.
- Catalog page text, mkdir form, folder tiles, escrow page, and help
  layout now resolve every visible string through the i18n table.
  Health, inspect, similar, and diff pages get translated topbars + nav;
  their dense per-(channel, layer) detail labels remain English for now.

## [0.3.0] - 2026-06-21

### Changed
- **Stage 8 — Leptos 0.7 upgrade.** `holofs-web` migrated from Leptos 0.6 to
  0.7: `use leptos::prelude::*` everywhere, `<Routes fallback=…>` required,
  `path!("/foo")` macro for route matchers, `Resource::new` / `RwSignal::new`
  / `Effect::new` replace the old `create_*` builders, `mount::hydrate_body`
  replaces `mount_to_body`, branched view arms type-erase via `.into_any()`,
  `leptos_axum::render_app_to_stream` takes only the app fn, and
  `get_configuration` is now synchronous. The dev WASM bundle grew slightly
  (≈7.6 MB) but `--release` + `wasm-opt` lands around 1.04 MB.
- **MSRV bumped 1.75 → 1.81.** Required by tokio 1.41+, tracing-subscriber
  0.3.19+, and hyper 1.5+ — all common upper bounds across the tree.
- **Wire-protocol PEM parsing now uses `rustls::pki_types::pem`.** Dropped
  the unmaintained `rustls-pemfile` crate; `holofs-storage::tls` calls
  `CertificateDer::pem_slice_iter` / `PrivateKeyDer::from_pem_slice` from
  `rustls-pki-types 1.x` instead.

### Removed
- `rustls-pemfile` workspace dependency.
- Stale advisory ignores in `deny.toml`: `RUSTSEC-2024-0370`
  (`proc-macro-error 1.x` — dropped with Leptos 0.7) and `RUSTSEC-2025-0134`
  (`rustls-pemfile` — dropped with the pki-types switchover). The remaining
  two ignores (`paste`, `proc-macro-error2`) are still pulled in by Leptos
  0.7 transitives.
- The `=0.6.13` pin on `server_fn` and the `=0.3.64` pins on `web-sys` /
  `js-sys` — Leptos 0.7 ships a clean `server_fn 0.7` build, so the
  workarounds for the broken 0.6.15 WASM browser code path are no longer
  needed.

## [0.2.0] - 2026-06-22

### Added
- **Stage 4 — Leptos SSR migration.** New `holofs-web` binary (axum + Leptos
  0.6 + WASM hydrate) replaces the hand-rolled HTTP gateway. Server functions,
  per-route SSR components, reactive `/health` cluster dashboard fed by an SSE
  endpoint (`GET /api/health/events`).
- **Stage 5 — operational shape.** `clap`-derive CLI parser on `holofs-web`
  with `HOLOFS_*` env fallbacks; structured logging via `tracing` +
  `tracing-subscriber` (text / JSON formats); pull-based Prometheus
  `/metrics` endpoint exposing node and catalog gauges; `tower-http`
  `TraceLayer` for per-request spans.
- **Stage 6 — wire-protocol TLS.** rustls 0.23 over `tokio-rustls`; opt-in
  `--tls` and `--mtls` flags. Embedded mode auto-generates a self-signed CA
  + per-leaf certs via `rcgen`; distributed mode reads operator-supplied
  PEM files. `holofs_client::transport` wraps both plain TCP and TLS.
  `deny.toml` tightened to MIT/Apache/BSL allowed-list with explicit
  ignores for the Leptos 0.6 unmaintained-dep chain.
- **Stage 7 — release pipeline.** `cargo leptos build --release` produces
  a `wasm-opt`'d 800 KB hydrate bundle; Dockerfile rewritten around it,
  multi-arch via `docker buildx` (linux/amd64 + linux/arm64); release
  workflow tarballs the WASM site bundle alongside the binaries.

### Changed
- Bumped workspace version `0.1.0 → 0.2.0` to reflect Stage 4-6 breaking
  changes (the public `Gateway` API surface was rewritten).
- `[workspace.package].license = "MIT OR Apache-2.0"` (was `"TBD"`).
- All workspace crates now opt into `publish = false`; will flip back to
  `true` per-crate once individual public APIs stabilise.
- Comments and identifier docs across every crate translated to English.

### Removed
- `holofs-http` binary and the entire hand-rolled HTTP/1.1 parsing layer
  (~2,600 lines from `crates/holofs-gateway`).
- Legacy `--addr` / `--storage` / `--photo` flag set on the gateway —
  superseded by the `clap` surface on `holofs-web` (env-var fallbacks
  preserved).

## [0.1.0] - 2026-06-20

### Added
- Cargo workspace reorganized into 10 crates (`holofs-core`, `-model`, `-wire`,
  `-codec`, `-storage`, `-client`, `-cluster`, `-analytics`, `-gateway`, `-cli`,
  plus `-web` placeholder for the upcoming Leptos UI).
- Workspace-level lints (`#![deny(unsafe_code)]`, `#![warn(missing_docs)]`,
  clippy `pedantic`) enforced via `[workspace.lints]`.
- `rustfmt.toml`, `clippy.toml`, `deny.toml` configs at the workspace root.
- GitHub Actions CI: fmt, clippy (stable + beta), tests on Linux / macOS /
  Windows, MSRV check (1.75), rustdoc with broken-link detection,
  cargo-deny and cargo-audit, optional code coverage via cargo-llvm-cov.
- Release workflow: cross-platform release binaries
  (Linux x86_64/musl/aarch64, macOS x86_64/aarch64, Windows x86_64) and
  multi-arch Docker images published to GHCR on `v*.*.*` tags.
- `Dockerfile` (multi-stage, distroless-style runtime, non-root user, tini PID 1).
- Helm chart `deploy/helm/holofs` with StatefulSet + PVC + Service + optional Ingress.

Initial pre-release covering 10 development stages: core RLNC + DWT + Merkle
pipeline, persistent multi-process cluster with Ed25519 identity and
admin-signed whitelist, HTTP gateway with admin UI, holographic key escrow,
perceptual fingerprint and MinHash search, per-chunk diff, shard inspector.
185 tests passing.
