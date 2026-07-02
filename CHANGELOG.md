# Changelog

All notable changes to holofs are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed — Phase R2: holofs-web module decomposition

- **`crates/holofs-web/src/lib.rs` shrunk from 2487 → 253 lines
  (~90%).** The historical file (Leptos SSR components +
  server_fns + filter + view-models all in one module) split
  into four sibling modules:
  - `catalog_types` — `CatalogEntry` view-model shared across
    SSR + hydrate boundaries, plus SSR-only `from_manifest`.
  - `filter` — Stage 11.17 catalog filter (`CatalogFilter`,
    `apply_filter`, `compile_glob`, `parse_date_to_unix`,
    `ymd_to_unix`) + seven unit tests.
  - `server_fns` — the three `#[server]` functions
    (`get_catalog`, `list_dir`, `list_dir_page`) + `ListDirPage`
    + `TreeSort` + `compare_entries`.
  - `catalog_ui` — the fifteen Leptos components
    (`CatalogPage`, `CatalogFocusView`, `FilterBar`,
    `TreeZoomButtons`, `Breadcrumb`, `CatalogTreeView` + eager /
    lazy variants, `LazyLevel`, `LazyDirNode`,
    `CatalogTreeBody`, `TreeNodeView`, `MkdirForm`, `UploadForm`,
    `ObjectCard`).
  What remains in `lib.rs`: module registry + crate-root
  `pub use` re-exports + `Shell` / `App` / `RoutedApp` top-level
  components + `url_encode` utility + WASM `hydrate` entry.

- **`crates/holofs-web/src/handlers.rs` shrunk from 1857 → 64
  lines (~96%).** Every axum handler moved into a domain
  submodule under `handlers/`:
  - `handlers/objects` — GET / PUT / DELETE `/*path`, preview,
    streaming preview, `/api/shard/…`, wasm alias.
  - `handlers/dirops` — mkdir, rmdir, rm, mv (JSON + form).
  - `handlers/uploads` — multipart `/api/upload`.
  - `handlers/versions` — `/api/restore`, `/api/versions/delete`.
  - `handlers/analytics` — `/api/fingerprint/*`, `/api/mix.png`,
    `/api/mix-save`, `/api/spotlight.png`.
  - `handlers/search` — `/api/embed_all`, `/api/search`.
  - `handlers/health` — `/api/stats`, `/metrics`, `/api/gc`,
    `/admin/node`, `/api/health/events` SSE.
  - `handlers/escrow` — `/escrow/{split,download,recover}`.
  - `handlers/util` — pure helpers (form parsing, JSON escape,
    path validation, `error_to_response`, header shortcuts).
  - `handlers/response` — response builders (range serving,
    ingest / remove / mkdir / rmdir / rename → HTTP, stats +
    fingerprint → JSON).
  Public API preserved via `pub use handlers::foo` re-exports.

Together with the R1 gateway split (v0.6.0), no source file in
the workspace should now materially exceed 500 lines — the
"god-module" era is over. See `docs/architecture.md § 1.3` for
the full module map and the "rule of thumb" for where new
components / handlers belong.

## [0.6.0] - 2026-07-02

### Changed — Phase R1: gateway monolith decomposition

- **`http_gateway.rs` shrunk from 4477 → 288 lines (~93.6%).** The
  historical god-module split into 18 single-purpose siblings under
  `crates/holofs-gateway/src/`:
  `decode`, `diff`, `dirops`, `error`, `escrow`, `fingerprint`, `gc`,
  `health`, `ingest`, `inspect`, `metrics`, `mix`, `repair`, `search`,
  `similarity`, `spotlight`, `util`, `versions`. Each module owns one
  `impl Gateway { ... }` block. The `Gateway` struct + accessors +
  `persist_catalog` / `invalidate_cache` are all that remain in
  `http_gateway.rs`. Public API preserved via `pub use` at the crate
  root, so `holofs_gateway::GatewayError`, `SimilarReport`,
  `FileMetrics`, etc. still resolve without touching the module path.
- **`holofs-testutils` crate** extracted from three copies of the
  same `DisablePool` + `spawn_mock_node` helper across the client,
  transport, and audit test suites. Consumed as `[dev-dependencies]`.

### Added — Phase N1-N8: reliability layer

- **N1 — graceful shutdown.** SIGTERM / SIGINT (Ctrl-C on Windows)
  now drains axum + the three long-running loops + the 40 embedded
  node listeners in under a second. Coordinated via a shared
  `tokio_util::sync::CancellationToken`. `axum::serve.with_graceful_shutdown`
  stops accepting new connections when the token fires and waits
  for in-flight requests; the background loops honour the token
  inside `tokio::select!` around every tick and every sleep.
- **N2 — supervised background tasks.** New
  `holofs_web::supervised::supervised_spawn(name, shutdown, counter, f)`
  spawns the inner future as a child `tokio::task` and joins on it.
  A panic → ERROR log + exponential backoff (1 → 2 → 4 → 8 → 16 → 30 s
  cap) + restart. Wired into the monitor / auditor / scrub loops in
  bootstrap. Pre-N2 those loops advertised "suppresses per-tick
  panics" in their docs but never actually caught anything — a bug
  under `tick_once` silently killed the whole loop until an operator
  noticed a metric had stopped moving.
- **N3 — bounded-concurrency backpressure.** Two semaphores per
  route bucket:
  - **MEDIUM** (default cap 64, `HOLOFS_MEDIUM_CONCURRENCY`) — decodes,
    PUT, directory ops.
  - **LONG** (default cap 8, `HOLOFS_LONG_CONCURRENCY`) — semantic
    search, spotlight, `POST /api/gc`.
  On saturation the middleware returns `503 Service Unavailable`
  with a diagnostic body instead of piling axum tasks onto the
  runtime. SHORT bucket (`/api/stats`, `/metrics`) and streaming
  endpoints (SSE, `/preview/stream/*`) intentionally unbudgeted.
- **N4 — fail-loud catalog persistence.** `Gateway::persist_catalog`
  used to swallow IO errors via `eprintln!` and let the caller
  succeed anyway; a disk-full incident only surfaced hours later at
  the next restart. Now returns `Result<(), GatewayError::Persist>`
  which maps to `500 Internal Server Error`. Every writer path
  (`ingest_bytes`, `mkdir`, `rmdir`, `rename`, `remove_object`,
  `restore_version`) propagates. `Directory::save_atomic` also gained
  a race-free unique-tmp path (`.tmp.<pid>.<counter>`) so parallel
  writers no longer collide on a shared tmp filename.
- **N5 — persistent node reputation.** `Reputation` gained
  `encode` / `decode` / `save_atomic` / `load_or_new`. Wire format:
  `MAGIC | u32 n | f32 alpha | f32*n scores`. Bootstrap loads
  `<storage>/reputation.bin` if present (falling back to a fresh
  table on missing / corrupt / `n_nodes`-mismatched file). New
  supervised task `reputation-persist` snapshots the shared state
  every `HOLOFS_REPUTATION_PERSIST_INTERVAL` seconds (default 30)
  and once more on shutdown so the last observations survive a
  restart.
- **N6 — admin bearer-token auth.** `POST /admin/node` and
  `POST /api/gc` are gated by
  `Authorization: Bearer $HOLOFS_ADMIN_TOKEN` when the env var is
  set. Missing header → 401 (missing). Wrong token → 401 (bad).
  Env var unset → 403 (disabled) — safe-by-default; dev override
  via `HOLOFS_ADMIN_UNAUTHENTICATED=1` at the cost of a WARN at
  boot. Split by rejection reason in
  `holofs_admin_auth_failures_total{outcome=…}`.
- **N7 — per-route HTTP handler timeouts.** Three buckets:
  - **SHORT** (10 s) — `/api/stats`, `/metrics`, `/admin/node`.
  - **MEDIUM** (60 s) — decodes, PUT, dir ops, mix, diff, inspect,
    escrow.
  - **LONG** (5 min) — `/api/search`, `/api/spotlight.png`,
    `/api/gc`, `/api/embed_all`, `/api/fingerprint/*`.
  Streaming endpoints (SSE, multipart/x-mixed-replace) + MCP
  intentionally unbudgeted (the timer would start on the first byte
  and kill an SSE stream at the deadline). Elapsed → 504 Gateway
  Timeout with a diagnostic body.
- **N8 — observability metrics.** `/metrics` extended with the
  N-series counters:
  ```
  holofs_catalog_persist_failures_total
  holofs_handler_timeouts_total{bucket="short|medium|long"}
  holofs_backpressure_rejected_total{bucket="medium|long"}
  holofs_backpressure_permits_available{bucket="medium|long"}   (gauge)
  holofs_supervised_task_restarts_total{task="monitor|auditor|scrub"}
  holofs_admin_auth_failures_total{outcome="missing|bad|disabled"}
  ```

### Env vars added in v0.6.0

| Var | Default | Effect |
|---|---|---|
| `HOLOFS_MEDIUM_CONCURRENCY` | 64 | N3 permits for the MEDIUM bucket. |
| `HOLOFS_LONG_CONCURRENCY` | 8 | N3 permits for the LONG bucket. |
| `HOLOFS_REPUTATION_PERSIST_INTERVAL` | 30 (s) | N5 save cadence. |
| `HOLOFS_ADMIN_TOKEN` | _(unset)_ | N6 bearer token; sets the required `Authorization: Bearer …` value. |
| `HOLOFS_ADMIN_UNAUTHENTICATED` | _(unset)_ | N6 dev override — set to `1` to leave /admin + /api/gc open. |

### Tests

- 329 workspace + 56 SSR-only (holofs-web features=ssr) + 106 e2e =
  **491 tests, all green.** Adds ~90 new unit tests covering the
  reliability primitives and the module refactor.

## [0.5.0] - 2026-06-30

### Added — Stage 15.x: reliability + e2e coverage

- **Typed wire layer.** `holofs-client::rpc` runs every RPC inside
  `tokio::time::timeout` with an 8 s default cap (`HOLOFS_RPC_TIMEOUT_MS`
  env knob, `0` disables). On expiry the pooled stream is poisoned —
  the in-flight write/read got cancelled mid-frame so the byte
  boundary is undefined — and the timeout surfaces to the caller.
  `is_likely_stale_connection` was renamed `is_likely_transient` and
  grew a `TimedOut` arm, so the existing single-retry path also
  covers slow peers.
- **Typed `ClientError`.** `Protocol(String)` split into
  `RemoteError(String)` (remote acknowledged + reported failure) and
  `UnexpectedResponse { expected: &'static str, got: String }` (wire
  protocol mismatch). New `ClientError::is_timeout()` classifies the
  Phase-4 path without re-parsing strings.
- **`NoLiveNodes` panic fix.** `placement::place` /
  `place_layer_zone_aware` / `Manifest::place_shard` now return
  `Result<_, NoLiveNodes>` instead of `assert!`ing on an empty live
  slice. PUT against a fully-down cluster used to crash the gateway;
  it now surfaces as `GatewayError::ClusterDegraded` → HTTP 503.
- **Auto-repair-on-read.** GET path is wrapped in
  `decode_with_autorepair`: on `ClientError::LayerLost` it kicks
  `repair_object_inplace` (per-node surgical repair via
  `list_node_hashes` + `repair_node`), persists the mutated manifest,
  and retries the decode once. Counters in `/api/stats`:
  `auto_repairs_total`, `auto_repair_failures_total`.
- **Background shard scrub.** Tokio task ticks every
  `HOLOFS_SCRUB_INTERVAL` (default 600 s, `0` disables). Walks the
  catalog, diffs `list_node_hashes` vs `place_shard`, surgically
  repairs the mismatches before users hit them. Counters:
  `scrub_runs_total`, `scrub_repairs_total`. Coordinated with PUT /
  GC via a shared `gc_barrier` RwLock.
- **Per-object version deletion + retention.**
  `Gateway::delete_version(name, id)` drops a `.bin` archive and
  GC's its uniquely-held shards via `purge_orphans_of`. Form-friendly
  handler at `POST /api/versions/delete`. New env knob
  `HOLOFS_VERSIONS_KEEP_LAST=N` prunes the oldest archives on every
  PUT so each name's history stays bounded. UI: per-row "delete"
  button next to "restore" on `/versions/<name>`.
- **/api/upload integrated into the catalog tree.** Multipart upload
  form sits next to the per-folder mkdir form on every `<details>`
  row, and the root toolbar gained a matching upload form (parent="").
  No more detour through a separate Upload page; `return_to=/?p=...`
  brings the user back to the same folder.
- **"Open" cds into the folder.** Catalog tree now renders rooted at
  the chosen folder when the URL carries `?p=<path>`. Top-level
  entries are the folder's children, breadcrumb at the top, mkdir +
  upload scoped to the current root. The earlier focus-view page
  (`CatalogFocusView`) is no longer dispatched; old `?p=` bookmarks
  still work as the new root selector.
- **Stage 14.0 — orphan-shard garbage collector.** `POST /api/gc`
  walks the catalog + version archives, lists every node's held
  hashes, computes the diff, and asks each node to `PurgeByHash` the
  orphans. Per-node breakdown in the response; held / orphaned / ok /
  error per addr. Idempotent — running twice on a clean cluster
  reports zero on the second pass.
- **Stage 14.2 — HNSW-backed semantic search.** Previously
  brute-force scan of `embeddings.bin` per query; now lazily builds
  an `instant_distance::HnswMap` per band, cached across queries
  until the next PUT bumps `ann_generation`. Threshold 200 vectors —
  smaller bands still brute-force inside the index for sub-ms
  latency. Build cost on M-series Macs: ~80 ms for 1k vectors,
  ~1.4 s for 50k. Public contract unchanged.
- **CLIP-multilingual embeddings.** Swapped `clip-ViT-B-32` for
  `sentence-transformers/clip-ViT-B-32-multilingual-v1`
  (DistilBERT + 768→512 projection). `/search?q=ocean` and
  `/search?q=океан` now hit the same images.
- **Stage 13.4 — per-object version history.** `--enable-versions`
  archives every PUT-replaced manifest as a side file under
  `<storage>/versions/<sanitized_name>/v<ts>_<cid>.bin`.
  `GET /versions/<name>` renders the timeline; `POST /api/restore`
  swaps the catalog entry without touching shards.
- **Stage 13.5 — `/help` Mermaid + KaTeX assets via local proxy.**
  jsDelivr fetch only on `/help` routes; everywhere else the page
  doesn't pay the bundle weight.
- **Stage 13.2 — `/spotlight` ROI.** Sharp inside an axis-aligned
  box, smooth outside (only L0 coefficients fetched outside the ROI).
  Both pixel-space (`x_px`/`y_px`/`w_px`/`h_px`) and normalised
  (`x`/`y`/`w`/`h`) coordinates accepted.
- **Stage 13.1 — streaming `/holo`.** `multipart/x-mixed-replace`
  body re-rendered for every layer L0 → L_max so the browser shows
  the progressive reveal.
- **Stage 13.0 — `/diff`, `/similar` and per-image health.** Three
  SSR pages on top of MinHash / perceptual-fingerprint /
  `object_health` server functions. Diff produces a per-chunk
  PSNR/MSE map; similar ranks the top-K by Hamming distance over
  the 45-bit dHash; health emits the per-(channel, layer) margin
  table.
- **Stage 12.8 — `/search` page.** Natural-language query against
  CLIP embeddings; per-result cards with score + band + thumbnail.
  Disabled when the gateway boots without `--enable-embed`
  (returns 503 with a hint).
- **Stage 12.7 — per-file metrics.** `/api/file_metrics/<name>` and
  the `/health/<name>` page; per-channel-per-layer alive / margin /
  detail-score block.
- **Stage 12.0–12.1 — MCP server.** Streamable-HTTP Model Context
  Protocol endpoint at `/mcp`, read-only by default; setting
  `HOLOFS_MCP_TOKEN` flips on write tools behind bearer auth.
- **End-to-end test suite — 106 tests across 32 files.** Browser
  harness (thirtyfour + chromedriver) plus HTTP-only suites:
  - UI flows: catalog, about, actions, diff, escrow, health, help,
    holo, i18n, inspect, mix, search, similar, spotlight, versions.
  - Reliability regressions: dedup safety, audit reputation,
    catalog tree, auto-repair, multilingual search.
  - Concurrency: parallel PUTs, PUT-vs-GET, PUT-vs-GC.
  - API negative paths: 4xx error mapping across PUT/GET/DELETE/
    mkdir/rmdir.
  - API semantics: /api/stats invariants, /api/gc idempotence,
    /metrics Prometheus shape, /api/mv rename.
  - Cluster-degraded paths: kill-all → 503, partial loss survives,
    recovery via /admin/node toggle.
  - Content roundtrip per kind: UTF-8 text, audio WAV, opaque
    blobs, preview semantics.
  - Auto-repair end-to-end: counter movement under controlled
    node loss.
  - Persistence across `TestHarness::restart()`: bytes, stats,
    nested directory tree, versions history.
- **`HarnessConfig::extra_env`.** Lets a single e2e test set env
  knobs on the spawned gateway (used for `HOLOFS_NO_SEED`,
  `HOLOFS_VERSIONS_KEEP_LAST`, etc.) without touching the test
  runner's process env.
- **Workspace test coverage refresh.** Unit tests across
  `holofs-wire`, `holofs-embed/index.rs`, `holofs-cluster/audit`
  + `monitor`, `holofs-client/client` + `pool` + `transport`,
  `holofs-storage/tls`, `holofs-codec/audio_codec`. Total
  workspace + e2e: 426 green.
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

### Fixed — Stage 15.x

- **DELETE on dedup'd siblings no longer kills the survivor.** The old
  `purge_object` issued `Request::Purge { object_id }` which wipes the
  entire `(object_id, channel, layer)` bucket on each node — when two
  catalog entries shared a `data_cid` (and therefore `object_id`),
  deleting one yanked shards out from under the other. The new
  `purge_orphans_of` walks the rest of the catalog plus the on-disk
  version archives, builds the set of hashes still referenced after
  the target manifest is conceptually removed, subtracts from
  `manifest.shard_hashes`, and `PurgeByHash`'s only the residue.
- **Audit reputation no longer cascades.** `AuditOutcome::MissingShard`
  used to count as a failure observation; on synthetic 256×256 PNGs
  upscaled to 512×512 by the gateway, layer-3 all-zero systematic
  shards hash-collide across unrelated images and `place_shard`
  keeps pointing at the *canonical* node while the bytes live on a
  *dedup* node. The cascade dropped every node's reputation below
  threshold; decode 503s spread across the catalog overnight.
  MissingShard is now neutral; HashMismatch is the only failure
  signal the auditor produces, with the monitor's per-layer margin
  as the corroborating telemetry.
- **Synthetic PNG dedup collisions.** `tools/test-data/generate-samples.py`'s
  `write_png` now injects per-image deterministic ±1 LSB jitter
  (seed = CRC32 of file path) so high-frequency DWT shards are
  unique. Without the jitter the smooth synthetic generators
  (mandala / gradient / coloured-shapes / brand-pairs) produced
  all-zero layer-3 coefficients across 22 of 29 PNGs, hash-collided
  into a single canonical shard per (channel, layer), and only the
  *first* image to be PUT could ever decode.
- **Lazy catalog tree no longer hangs on first paint.** Replaced the
  `Effect::new(...) + spawn_local` lazy loader with a keyed `Resource`;
  the previous pattern didn't run its initial pass under streaming
  hydrate, so depth-0 folders with `initial_open=true` sat under a
  permanent "loading catalog…" placeholder.
- **Catalog tree expand/collapse race.** `holofsExpandAll` /
  `holofsCollapseAll` got an epoch guard so a click that landed mid
  re-render of a lazy `<details>` doesn't get reverted by the
  next refresh.
- **Folder "open" link.** Six anchors in the catalog tree pointed at
  `/?p=` without `rel="external"`; leptos' SPA router intercepted
  them and the page never reloaded. Added `rel="external"` to every
  catalog-side anchor that crosses the focus / tree boundary,
  including the breadcrumb Home link.
- **Breadcrumb Home was a no-op.** Same rel-external story as above,
  but specifically on the Home anchor inside `Breadcrumb` so
  clicking "Home" from `/?p=audio` actually goes back to `/`.
- **E2E harness sends SIGTERM, not SIGKILL.** `kill_and_wait` now
  shells out to `/bin/kill -TERM <pid>` first and polls 500 ms
  before falling back to the existing SIGKILL. Graceful shutdown
  lets the gateway flush the catalog (and anything else atexit-y);
  the original motivation was capturing `.profraw` from coverage-
  instrumented runs, which still doesn't work end-to-end because
  the gateway has no SIGTERM handler — but the cleaner shutdown
  stands on its own merit.
- **`generate-samples.py` no longer wipes `landscapes-xl/`.** The
  picsum.photos fetch script is independent and writes there; the
  generator now backs up and restores that folder around its
  `shutil.rmtree(out_root)` instead of blowing it away.
- **`/api/search` reads `?q=` correctly.** Empty / whitespace-only
  queries reject with 400 before paying the CLIP-encode cost;
  missing `q=` is a 400 instead of a confusing CLIP-init error.

### Fixed — earlier
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
