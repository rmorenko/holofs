# holofs

**A holographic distributed file system for trusted clusters.**

No single node stores a byte of the original. Each file lives as a swarm of
random linear projections ("shards") spread across the cluster. Any
sufficient handful of shards reconstructs the data exactly; an insufficient
handful still yields the same data, just at a lower resolution. Like a piece
of a hologram: cut it in half and the picture remains whole, just blurrier.

> Status: working prototype, **v1.0.0**. Not for production. The
> full feature surface — axum + Leptos SSR web UI, persistent
> multi-process cluster with Ed25519 identity, signed admin
> whitelist, opt-in rustls + mTLS on the wire, hierarchical
> catalog with directory objects, per-object version history
> with deletion + retention cap, per-block replicated encoding
> with bandwidth-aware ROI fetch, perceptual search via
> HNSW-backed CLIP embeddings, holographic spotlight (sharp
> inside an ROI, smooth outside), streaming progressive `/holo`,
> per-file health + diff + similar pages, auto-repair-on-read
> with surgical per-node fixes, background shard scrub,
> epoch-based garbage collection, typed RPC layer (timeouts +
> retries + `NoLiveNodes`), Shamir-style key escrow,
> AES-256-GCM at-rest shard encryption, TOML configuration
> file, per-IP rate limiter, streaming PUT with tempfile spill,
> MCP server for LLM tooling, in-app docs viewer with Mermaid +
> KaTeX, and UI in five languages (English, Russian, German,
> French, Spanish). Reliability layer: SIGTERM/SIGINT graceful
> shutdown, supervised background tasks with panic-catching +
> exponential-backoff restart, bounded-concurrency backpressure
> per route bucket, fail-loud catalog persist, persistent node
> reputation across restarts, admin bearer-token auth, per-route
> handler timeouts, Prometheus counters
> (`holofs_catalog_persist_failures_total`,
> `holofs_handler_timeouts_total{bucket}`,
> `holofs_backpressure_rejected_total{bucket}`,
> `holofs_backpressure_permits_available{bucket}`,
> `holofs_supervised_task_restarts_total{task}`,
> `holofs_admin_auth_failures_total{outcome}`,
> `holofs_rate_limit_rejected_total`).

---

## Documentation

The repository ships a full documentation set under [`docs/`](./docs/),
translated into five languages (en / ru / de / fr / es):

| Document | Audience |
|---|---|
| [docs/theory.md](./docs/theory.md) | engineers, researchers — math foundations (GF, RLNC, DWT, MinHash) |
| [docs/architecture.md](./docs/architecture.md) | maintainers — system structure, data flow, crate map |
| [docs/api.md](./docs/api.md) | integrators — HTTP API, wire protocol, on-disk formats |
| [docs/operations.md](./docs/operations.md) | operators — deploy, configure, monitor, recover, TLS |
| [docs/threat-model.md](./docs/threat-model.md) | security reviewers — STRIDE + LINDDUN analysis |
| [CHANGELOG.md](./CHANGELOG.md) | release-by-release deltas (Keep-a-Changelog, SemVer) |

Localized variants live under `docs/<lang>/*.md`. The running binary
serves all of them at **`/help`** — Mermaid diagrams and KaTeX math are
rendered inline, the sidebar lets you switch document or language without
leaving the page.

Most of the prose that used to live in this README has migrated into those
documents — start with [`docs/architecture.md`](./docs/architecture.md) for
a system tour or [`docs/operations.md`](./docs/operations.md) for a
hands-on cluster walkthrough.

---

## Why holofs

Holofs solves **one specific problem**: how to store valuable media so that
when a large fraction of the cluster fails, the client still gets a
**recognisable result** rather than nothing. It is a backup target for
photo archives, X-rays, surveillance footage, satellite imagery —
content that should survive a disaster at *some* quality, not none.

### Effect at a glance

|                                  | Classic erasure code (Reed-Solomon, Storj) | Holofs                              |
|----------------------------------|--------------------------------------------|-------------------------------------|
| Cluster 100% alive               | full file                                  | full file                           |
| Lose 25% of nodes                | full file                                  | full file in 87% of runs            |
| Lose 50% of nodes                | full file (if above threshold K)           | **recognisable image** in 78% of runs |
| Lose 75% of nodes                | **0 bytes recoverable**                    | rough shape in 12% of runs          |
| Whole rack / AZ failure          | one entire layer lost                      | only the fine detail is lost        |

Numbers come from a Monte-Carlo run on 40 nodes across 4 zones —
reproducible via `/health/photo.png` in `holofs-web`.

### What the design buys you

- **Guaranteed minimum quality** instead of all-or-nothing. Below the
  decode threshold holofs still serves data **at lower resolution** —
  enough for triage X-rays, overview tiles, thumbnails.
- **Progressive reads.** A coarse preview lands in ~15 ms on ~200 KB of
  traffic (≈5% of the full object). Full quality streams on demand.
- **Per-layer durability budget.** The coarse layer is stored at high
  redundancy (×4); fine detail at low (×1.15). You pay more for what
  matters most.
- **Cheap repair.** Replacing a node costs **9× less CPU** than a full
  rebuild (RLNC regeneration straight from live shards).
- **Zone-aware placement.** Anti-affinity spreads shards across racks/AZs
  so a whole-zone failure can never erase an entire layer.
- **Leak resistance.** No node holds the bytes of any file — only random
  linear combinations. Exfiltrating one node yields no original.
- **Perceptual search without decompression.** The L0 systematic shards
  already are "low-res in the frequency domain"; a 16-byte fingerprint is
  computed from them without decoding the file.
- **Shamir-style secret sharing out of the box.** The same RLNC over
  GF(256) is a threshold scheme — "any 3 of 5 family devices restore the
  seed phrase".

For the full theory see [`docs/theory.md`](./docs/theory.md); for the
trust model and what holofs deliberately doesn't defend against see
[`docs/threat-model.md`](./docs/threat-model.md).

---

## What works today

Supported object kinds and how they degrade:

| Kind         | Encoding                                  | Degradation under loss                            |
|--------------|-------------------------------------------|---------------------------------------------------|
| **Image**    | 2D Haar DWT + 4 priority layers + RLNC    | Blurrier (high frequencies disappear first)       |
| **Audio**    | 1D Haar DWT + 4 priority layers + RLNC    | Muffled (treble disappears first)                 |
| **Text**     | UTF-8 chunked + 1 layer + systematic RLNC | Holes at lost positions (the rest stays readable) |
| **Opaque**   | 1 RLNC layer, no DWT                      | All-or-nothing at the K threshold (Reed-Solomon)  |
| **Directory** | manifest-as-marker, no shards | path-resolution metadata only                     |

Upload formats are auto-detected:

- **Image**: PNG, JPEG, WebP, GIF, BMP, TIFF
- **Audio**: WAV, MP3, FLAC, OGG, AAC, M4A, AIFF (via `symphonia`)
- **Text**: any UTF-8 (txt, md, html, json, css, csv, …)
- **Anything else**: opaque mode (PDF, DOCX, ZIP, EXE, DB dumps, …)

### Hierarchical catalog

Catalog paths are slash-separated. Directories are first-class
manifest-backed objects with explicit `mkdir` / `rmdir` / `rename` and
form-friendly UI controls. The web UI navigates by query (`/?p=<prefix>`)
with breadcrumbs and folder tiles.

```sh
curl -X POST http://127.0.0.1:8787/api/mkdir/photos
curl -X POST http://127.0.0.1:8787/api/mkdir/photos/2026
curl -X PUT --data-binary @img.jpg \
     http://127.0.0.1:8787/photos/2026/img.jpg
curl http://127.0.0.1:8787/photos/2026/img.jpg > out.jpg
```

Wire shape, status codes, and reserved top-level segments are listed in
[`docs/api.md`](./docs/api.md).

---

## Quick start

> **holofs is always persistent.** There is no in-memory mode. Shards,
> node identities, and the catalog live on disk and survive restarts.

### Embedded cluster (40 nodes in one process)

```sh
cargo run --release --bin holofs-web
# open http://127.0.0.1:8787/
```

By default data lives under `./holofs-data/`:

- `./holofs-data/node_00`…`node_39` — shards and `identity.key` for each node
- `./holofs-data/catalog.bin` — object catalog

Common overrides (full CLI in [`docs/operations.md`](./docs/operations.md#5-configuration-reference)):

```sh
HOLOFS_STORAGE_DIR=~/my-holofs \
HOLOFS_LOG=info \
HOLOFS_LOG_FORMAT=json \
  cargo run --release --bin holofs-web
```

Embedded nodes bind to **stable ports** 9100..9139 (the base is overridable
via `HOLOFS_EMBED_BASE_PORT`). Stable ports are required for persistence:
ephemeral ports would shift on every restart and leave manifests pointing
at dead addresses.

### Multi-process cluster (one node per process)

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

The script spawns 8 `holofs-node` processes with persistent storage in
`.cluster-data/node-N/`, collects their Ed25519 pubkeys, generates an
admin keypair, signs the whitelist, and starts the gateway pointed at it.
Ctrl-C stops everything.

### Demo test data

For a realistic walkthrough of `/search`, `/similar`, `/spotlight`,
`/versions`, the cluster ships with a sample-tree pipeline under
`tools/test-data/`:

```sh
# 1. Six 2560×1440 picsum.photos JPEGs go under photos/landscapes-xl/
tools/test-data/fetch-real-landscapes.sh

# 2. 44 synthetic images / WAVs / text / opaque blobs around them
python3 tools/test-data/generate-samples.py

# 3. PUT the whole tree into a running gateway
tools/test-data/upload-samples.sh
```

The result is a 67-object catalog: 25+ images (real photos + synthetic
patterns), 10 WAVs, 7 text, 4 opaque, 17 directories. The synthetic
PNGs ship with deterministic LSB jitter so the highest-frequency DWT
shards stay unique per file — without it the smooth gradient / mandala
generators produce identical zero-coefficient shards across images
and the dedup layer collapses them onto one canonical node, breaking
every GET but the first.

### TLS / mTLS on the wire (optional)

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web
```

Embedded mode auto-generates a self-signed CA + per-leaf certs. For real
deployments pass `--tls-cert`, `--tls-key`, `--tls-ca-cert`. The bare-metal
install walkthrough in [`docs/operations.md`](./docs/operations.md#2-bare-metal-install)
shows the full PKI flow.

### Observability

- `GET /metrics` — Prometheus exposition (`text/plain; version=0.0.4`)
- `GET /api/health/events` — Server-Sent Events stream of cluster health
- `tower-http` `TraceLayer` emits one span per HTTP request; pair with
  `--log-format json` for ELK/Loki ingestion.

---

## Repository layout

The workspace is split into 14 crates under `crates/`. A full crate map
with each crate's responsibility lives in
[`docs/architecture.md`](./docs/architecture.md#1-crate-dependency-graph);
the short version:

| Crate | Role |
|---|---|
| `holofs-core` | GF(256), SHA-256, DWT, RLNC, Merkle — the math primitives |
| `holofs-model` | `Manifest`, `Directory`, `ObjectKind`, path module, placement |
| `holofs-wire` | binary frame protocol (tokio TCP, `[u32 len BE][payload]`) |
| `holofs-codec` | image/audio/text/opaque encoders & decoders |
| `holofs-storage` | per-node on-disk shard store, identity, whitelist, TLS scaffold |
| `holofs-client` | PUT/GET/REPAIR/AUDIT client + RPC timeouts + per-addr keepalive pool, TLS transport, live-node discovery |
| `holofs-cluster` | health monitor, PoR auditor, rebalancer, zone-aware placement, reputation |
| `holofs-embed` | CLIP-multilingual text/image embeddings + HNSW ANN index for `/search` |
| `holofs-analytics` | perceptual fingerprint, MinHash, escrow, chunk diff |
| `holofs-gateway` | catalog, decode pipeline, auto-repair-on-read, background scrub, public Gateway API |
| `holofs-mcp` | Model Context Protocol server (Streamable HTTP), read-only by default |
| `holofs-web` | axum + Leptos 0.7 SSR web UI, server functions, HTTP handlers |
| `holofs-cli` | `holofs-admin` (keys, whitelist), `holofs-bench`, `holofs-inspect`, `holofs-cluster`, `holofs-fs`, `holofs-node`, `holofs` |
| `holofs-e2e` | browser-driven (thirtyfour + chromedriver) end-to-end test harness |

Top-level: `Cargo.toml` (workspace), `Cargo.lock`, `Dockerfile`,
`deny.toml`, `.github/workflows/`, `assets/sample.png`,
`scripts/spawn-cluster.sh`, `deploy/helm/holofs/`, `docs/`, `CHANGELOG.md`.

---

## Tests

```sh
cargo test --workspace --exclude holofs-e2e         # 329 workspace tests
cargo test -p holofs-web --features ssr --lib       # 56 SSR-only lib tests
cargo test -p holofs-e2e -- --test-threads=1        # 106 e2e tests (needs chromedriver)
cargo deny check                                    # advisories + licenses + bans + sources
cargo clippy --workspace                            # workspace lints (pedantic-leaning)
```

The e2e suite spawns a fresh `holofs-web` gateway against a `TempDir`
storage for every scenario; running them serial (`--test-threads=1`)
keeps embedded node ports collision-free. Ten tests are
`#[ignore]`'d behind `--include-ignored` because they download the
~155 MiB DistilBERT-multilingual CLIP weights on first run.

Line-coverage on measurable code (excluding HTTP-handler / leptos
SSR code that only runs inside the spawned gateway):

```sh
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --exclude holofs-e2e --summary-only \
  --ignore-filename-regex \
  'tests/|holofs-e2e/|holofs-web/|holofs-cli/src/bin/|holofs-gateway/src/http_gateway\.rs|holofs-mcp/src/lib\.rs|holofs-embed/src/(model|text)\.rs|holofs-codec/src/image_io\.rs'
# TOTAL ≈ 91% line coverage across the measurable surface (v0.5.0
# baseline; v0.6.0 refactor moved a lot of code around without
# changing behaviour, so the number is approximately preserved).
```

The CI matrix runs the same set on stable + beta on Linux / macOS /
Windows plus an MSRV (1.81) check; the WASM hydrate bundle is built via
`cargo leptos build --release`. See [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)
for the full job graph and [`docs/operations.md`](./docs/operations.md#6-monitoring--alerting)
for production health/alerting plumbing.

---

## Boundaries and non-goals

- **Holographic (smooth) degradation is for media only.** Images and
  audio survive partial losses meaningfully thanks to the wavelet
  multi-resolution. Arbitrary binaries (DBs, archives) use opaque mode:
  classic erasure code, no gradient. Text uses partial recovery (holes
  at the lost positions), not "fuzzy text".
- **Trusted internal cluster only.** No Sybil resistance, no
  proof-of-storage à la Filecoin / Storj. Nodes authenticate with
  Ed25519; cluster membership is fixed by an admin-signed whitelist —
  Backblaze-level trust, not permissionless. See
  [`docs/threat-model.md`](./docs/threat-model.md).
- **Not in-place mutable.** Immutable / append-only by design.

---

## Dependencies

The full transitive set is pinned in `Cargo.lock` and audited via
`cargo deny`. Headline runtime deps:

| Crate | Role |
|---|---|
| `tokio` | async runtime, TCP |
| `axum`, `tower-http`, `leptos`, `leptos_axum` | HTTP + SSR |
| `ed25519-dalek`, `rand_core` | node identity |
| `image`, `symphonia` | universal image / audio decoders |
| `rustls`, `tokio-rustls`, `rcgen` | wire TLS + embedded CA |
| `clap`, `tracing`, `tracing-subscriber` | CLI + structured logs |
| `mime_guess`, `png`, `serde`, `serde_json` | I/O glue |

Everything else — SHA-256, GF(256), DWT, RLNC, multipart parser, WAV
encoder, MinHash, Ed25519 challenge-signing — is hand-rolled in
`holofs-core`, `holofs-storage`, `holofs-codec`, and `holofs-analytics`.

---

## License

Licensed under either of [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)
or [MIT license](https://opensource.org/licenses/MIT) at your option.
