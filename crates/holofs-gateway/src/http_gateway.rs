//! Holofs gateway core: the shared [`Gateway`] type + [`ClusterInfo`]
//! definition, its constructors and accessors, and the two catalog-
//! persistence helpers (`persist_catalog`, `invalidate_cache`) that
//! every writing path calls.
//!
//! Every substantive data-plane method lives in a sibling module:
//! `decode`, `ingest`, `dirops`, `search`, `versions`, `gc`, `escrow`,
//! `similarity`, `fingerprint`, `mix`, `diff`, `metrics`, `spotlight`,
//! `inspect`, `health`, `repair`. Each of those modules adds one
//! `impl Gateway {}` block. This file just owns the state.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use holofs_client::LiveNodes;
use holofs_core::gf::Gf;
use holofs_model::fs::Directory;
use holofs_model::placement::Placement;

use crate::health::ApiStats;
use crate::search::EmbedState;
use crate::versions::VersionsState;

/// Aggregate handle to every N-series counter for `/metrics`. Borrows
/// from the Gateway so read paths can format them without cloning
/// eleven Arcs. Consumers touch `.load(Ordering::Relaxed)` on each
/// atomic and `available_permits() / initial capacity()` on the two
/// semaphores.
pub struct ObservabilityCounters<'gw> {
    pub medium_permits: &'gw Arc<tokio::sync::Semaphore>,
    pub long_permits: &'gw Arc<tokio::sync::Semaphore>,
    pub medium_rejected_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub long_rejected_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub timeout_short_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub timeout_medium_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub timeout_long_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub task_restarts_monitor: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub task_restarts_auditor: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub task_restarts_scrub: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub catalog_persist_failures_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub admin_auth_missing_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub admin_auth_bad_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub admin_auth_disabled_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub rate_limit_rejected_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub objects_encoding: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub encode_completed_total: &'gw Arc<std::sync::atomic::AtomicU64>,
    pub encode_failed_total: &'gw Arc<std::sync::atomic::AtomicU64>,
}

/// N3: default MEDIUM-bucket concurrency (decodes, PUT, dir ops).
/// Chosen empirically — on the dev cluster (40 nodes × 444 shards)
/// a burst of ~40 concurrent decodes saturates the cluster. 64
/// gives headroom without letting a burst DoS the process. Callers
/// override via [`Gateway::configure_limits`] (bootstrap reads
/// `HOLOFS_MEDIUM_CONCURRENCY` and applies it).
///
/// The 50-worker soak study (`docs/operations.md §10.7`) tried
/// bumping this to 128 to unblock backpressure-storm scenarios;
/// the higher ceiling let more PUTs run concurrently, but PUT is
/// CPU-heavy (JPEG decode + DWT + RLNC fanout) and starved GET on
/// the same host — GET p50 jumped 1 ms → 79 ms, and cluster-wide
/// error rate went up, not down. 64 stays because PUT is the
/// greedy op that fills whatever bucket you give it.
pub const DEFAULT_MEDIUM_CONCURRENCY: usize = 64;

/// N3: default LONG-bucket concurrency. `semantic_search` +
/// `similar_to` + `spotlight` each walk the catalog and dispatch
/// N-shard fetches; running more than a handful in parallel just
/// serialises them on the shared shard cache and dropbox. 8 is a
/// reasonable ceiling for the dev cluster; production deployments
/// tune it via `HOLOFS_LONG_CONCURRENCY`.
pub const DEFAULT_LONG_CONCURRENCY: usize = 8;

/// Async-ingest encode-worker concurrency cap. RLNC over GF(2⁸) is
/// pure CPU; oversubscribing by 5× (which is what happens when
/// `HOLOFS_ASYNC_ENCODE=1` runs unthrottled — the HTTP handler
/// releases its MEDIUM permit the moment it emits 202, so
/// background workers accumulate) turns the tokio scheduler into
/// a context-switch storm and every other route starves.
///
/// A dedicated `encode_permits` semaphore keeps encoder parallelism
/// separate from HTTP MEDIUM: the 202 fast-path stays fast, excess
/// PUTs simply stay in `Encoding` longer, and polling clients see
/// 503+Retry-After and back off naturally. 8 matches typical
/// physical-core counts on dev / soak hardware; override via
/// `HOLOFS_ENCODE_CONCURRENCY`.
pub const DEFAULT_ENCODE_CONCURRENCY: usize = 8;

/// Async-ingest intake ceiling. When `objects_encoding` reaches
/// `DEFAULT_ENCODE_QUEUE_MAX` the 202 fast-path stops accepting new
/// PUTs and returns 503 + Retry-After instead. Prevents the queue
/// from growing without bound under a bursty client that hasn't
/// migrated to Retry-After polling yet: the whole point of async
/// ingest is fast-fail on overload, not silent RAM exhaustion.
/// Default is 4 × `DEFAULT_ENCODE_CONCURRENCY`; override via
/// `HOLOFS_ENCODE_QUEUE_MAX`.
pub const DEFAULT_ENCODE_QUEUE_MAX: usize = 32;

/// Cluster metadata needed to PUT a new object.
pub struct ClusterInfo {
    pub node_addrs: Vec<String>,
    pub zones: Vec<u8>,
    pub placement: Placement,
    pub width: usize,
    pub height: usize,
}

/// Shared gateway state — catalog, cluster topology, caches, and
/// self-heal counters. Constructed once at startup via [`Gateway::new`]
/// or [`Gateway::new_persistent`]; every axum handler in `holofs-web`
/// takes an `Arc<Gateway>` handle.
pub struct Gateway {
    // Every field is `pub(crate)` because the impl Gateway blocks are
    // split across sibling modules (search.rs, versions.rs, gc.rs, ...)
    // and each of those reaches into shared state. The Gateway type
    // itself stays `pub`; the fields don't leak outside the crate
    // boundary.
    pub(crate) catalog: Arc<RwLock<Directory>>,
    /// Optional path to the catalog file. If set, the catalog is saved
    /// atomically on each change (PUT/DELETE).
    pub(crate) catalog_path: Option<std::path::PathBuf>,
    /// optional semantic-search embeddings index. `None`
    /// when the server was started without `--enable-embed`. When set,
    /// every PUT fires a fire-and-forget background task that embeds
    /// the new object via CLIP and appends to the on-disk index.
    pub(crate) embed: Arc<Mutex<EmbedState>>,
    /// optional per-object version history. When enabled
    /// every PUT that *replaces* an existing object writes the prior
    /// manifest as a side file under `versions_dir/<sanitized>/v…bin`
    /// and skips the usual shard purge so the historical version
    /// remains decodeable. Trade-off: cluster storage monotonically
    /// grows while the feature is on (no GC yet).
    pub(crate) versions: Arc<Mutex<VersionsState>>,
    /// serialisation barrier between catalog-mutating
    /// writers and the orphan-shard GC pass.
    ///
    /// Writers (`ingest_bytes`, `restore_version`, `embed_object`)
    /// hold a `read()` guard for their full duration. The GC
    /// (`gc_orphaned_shards`) takes a `write()` guard, so it waits
    /// for every in-flight writer to finish AND blocks any new
    /// writer until it's done.
    ///
    /// Without this lock a fresh PUT that wrote shards to node N+1
    /// *after* GC's `ListHashes(node 0)` but *before*
    /// `ListHashes(node N+1)` would have its `h_new` show up in the
    /// held-list snapshot of node N+1 yet not in the live-hash set
    /// (snapshotted before the PUT updated the catalog) — and GC's
    /// `PurgeByHash(node N+1)` would silently delete the new shard.
    /// The barrier turns that race into "PUTs queue behind GC" which
    /// is fine for the manually-triggered `/api/gc`.
    pub(crate) gc_barrier: Arc<RwLock<()>>,
    pub(crate) gf: Arc<Gf>,
    /// Baseline list of "actually live" cluster nodes. `admin_kills` flags
    /// (set via the UI) are layered on top of it.
    pub(crate) live: Arc<LiveNodes>,
    /// "Node disabled by admin" flags indexed by `cluster.node_addrs`.
    /// The node keeps responding physically, but the gateway treats it as dead:
    /// PUT/GET bypass it, the health-monitor sees the margin drop, the auditor
    /// does not query it.
    pub(crate) admin_kills: Arc<Mutex<Vec<bool>>>,
    pub(crate) cluster: Arc<ClusterInfo>,
    /// Cache keyed by (name, max_decoded_layer) → ready PNG + metrics.
    pub(crate) cache: Mutex<HashMap<(String, u8), Arc<CachedFile>>>,
    /// per-`(name, channel, layer)` shard cache. `shard_payload`
    /// previously called `gather_layer` for every cell on `/inspect/<name>`
    /// — under the inspect grid's ~444 concurrent renders that saturated
    /// the cluster and produced spurious 404s for cells whose layer fetch
    /// raced. The `OnceCell` deduplicates concurrent gathers: the first
    /// caller does the work, every other caller awaits the same future.
    /// Invalidated together with [`Self::cache`] on every PUT / DELETE.
    pub(crate) shard_cache: Mutex<
        HashMap<(String, u8, u8), Arc<tokio::sync::OnceCell<Arc<Vec<holofs_core::rlnc::Shard>>>>>,
    >,
    /// Temporary cache of generated escrow shares: escrow_id_hex → Vec<ShareFile>.
    /// Kept only until the gateway restarts (shares are not part of the cluster).
    pub(crate) escrow_cache: Mutex<HashMap<String, Vec<holofs_analytics::escrow::ShareFile>>>,
    /// Perceptual-fingerprint cache. `compute_fingerprint` on
    /// image/audio does a per-channel `gather_layer` HTTP fanout to
    /// nodes — 3 channels × ~15 ms each = ~45 ms even under warm
    /// pool. `/api/similar/<name>` visits every same-kind
    /// neighbour, so the raw path is O(N × 45 ms) and hit ~32 s
    /// p50 on a 500-manifest catalog under a 50-worker soak.
    ///
    /// The fingerprint is a pure function of the manifest's
    /// content bytes; `manifest.object_id` is derived from the
    /// SHA-256 of exactly those bytes, so keying on `object_id`
    /// gives content-addressed cache semantics — a re-PUT of the
    /// same file hits the entry; a PUT-replace with different
    /// content just creates a fresh entry (the stale one is
    /// harmless clutter). No invalidation logic needed.
    pub(crate) fingerprint_cache:
        Mutex<HashMap<u64, holofs_analytics::fingerprint::Fingerprint>>,
    /// Cooldown table for auto-repair: `object_name → last_attempt`.
    /// `decode_with_autorepair` skips firing another `repair_node`
    /// pass on a name whose entry here is younger than
    /// [`AUTO_REPAIR_COOLDOWN`], returning the original `LayerLost`
    /// error to the caller immediately.
    ///
    /// Without this a soak-shaped read workload that hits a
    /// transient LayerLost on the same hot object N times in a
    /// row fires N full repair passes, each doing K shard-encode
    /// RPCs per node. On the July 2026 soak that showed up as
    /// 385 `auto_repair` attempts in 3 min sharing a small set
    /// of names, saturating the shared node HTTP pool and causing
    /// unrelated GET requests to time out at the transport layer
    /// (24 % of `get_random`). One retry per name per cooldown
    /// window is enough to heal a real hole while keeping the
    /// gateway responsive.
    pub(crate) auto_repair_cooldown: Mutex<HashMap<String, std::time::Instant>>,
    /// Auto-repair-on-read counters. Bumped from
    /// `decode_with_autorepair` when the first decode attempt
    /// hits `ClientError::LayerLost` and the retry path kicks in.
    /// Surfaced via `ApiStats`.
    pub(crate) auto_repairs_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) auto_repair_failures_total: Arc<std::sync::atomic::AtomicU64>,
    /// Background-scrub counters: how many objects this gateway has
    /// proactively repaired before any user GET tripped a 503.
    /// Bumped from the scrub task spawned at bootstrap (see
    /// `scrub_tick`).
    pub(crate) scrub_repairs_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) scrub_runs_total: Arc<std::sync::atomic::AtomicU64>,
    /// N4: how many times `persist_catalog` saw the on-disk write
    /// return an IO error. Pre-N4 this was swallowed via `eprintln!`
    /// and callers succeeded anyway; now they refuse with
    /// `GatewayError::Persist` and increment this counter. A
    /// non-zero value here means the catalog on disk is behind the
    /// catalog in memory and the next restart will lose writes.
    pub(crate) catalog_persist_failures_total: Arc<std::sync::atomic::AtomicU64>,
    /// N3: backpressure permits + rejection counters for the two
    /// non-cheap route buckets. `medium_permits` caps decode / PUT /
    /// dir-op concurrency (default 64); `long_permits` caps semantic
    /// search / spotlight / GC (default 8). Both configurable via
    /// `HOLOFS_MEDIUM_CONCURRENCY` / `HOLOFS_LONG_CONCURRENCY`.
    ///
    /// The `holofs-web` middleware wraps every request in a
    /// `try_acquire_owned` — on failure it bumps `*_rejected_total`
    /// and returns 503 Service Unavailable so a bursting client
    /// can back off instead of piling up axum tasks that all fight
    /// for the same 40-node cluster.
    pub(crate) medium_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) long_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) medium_rejected_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) long_rejected_total: Arc<std::sync::atomic::AtomicU64>,
    /// N7: 504 counter — bumped by the timeout middleware in
    /// `holofs-web` each time a handler exceeds its bucket deadline.
    /// Split by bucket via `holofs_handler_timeouts_total{bucket=…}`
    /// in `/metrics`.
    pub(crate) timeout_short_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) timeout_medium_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) timeout_long_total: Arc<std::sync::atomic::AtomicU64>,
    /// N2: supervised-task restart counters. Bumped every time
    /// `supervised_spawn` decides to restart after a panic
    /// (or an unexpected voluntary return). Emitted in `/metrics`
    /// as `holofs_supervised_task_restarts_total{task=…}`. Only
    /// the three long-lived tasks live here; ad-hoc supervised
    /// spawns would need to grow this map.
    pub(crate) task_restarts_monitor: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) task_restarts_auditor: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) task_restarts_scrub: Arc<std::sync::atomic::AtomicU64>,
    /// N6: admin bearer-token auth failures. Bumped by the
    /// `require_admin_token` middleware on 401 (bad or missing
    /// token) and 403 (admin surface disabled entirely). Emitted
    /// in `/metrics` as `holofs_admin_auth_failures_total{outcome=…}`
    /// split by rejection reason.
    pub(crate) admin_auth_missing_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) admin_auth_bad_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) admin_auth_disabled_total: Arc<std::sync::atomic::AtomicU64>,
    /// 429 responses caused by the per-IP rate limit
    /// middleware. Zero when the limit is disabled
    /// (`HOLOFS_RATE_LIMIT_RPS_PER_IP=0`). Emitted in `/metrics`
    /// as `holofs_rate_limit_rejected_total`.
    pub(crate) rate_limit_rejected_total: Arc<std::sync::atomic::AtomicU64>,
    /// Async-ingest lifecycle counters. Zero for the sync path
    /// (default). `objects_encoding` is a gauge — currently-active
    /// background encode tasks. `encode_completed_total` /
    /// `encode_failed_total` are cumulative counts of finished
    /// tasks by outcome. Emitted in `/metrics`.
    pub(crate) objects_encoding: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) encode_completed_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) encode_failed_total: Arc<std::sync::atomic::AtomicU64>,
    /// Dedicated backpressure for async-ingest background encoders.
    /// See [`DEFAULT_ENCODE_CONCURRENCY`] for why this is separate
    /// from `medium_permits` — reusing MEDIUM meant one saturating
    /// PUT storm blocked out mkdir / rmdir / put_new on their HTTP
    /// gate (97 % 503 in the July 2026 soak).
    pub(crate) encode_permits: Arc<tokio::sync::Semaphore>,
    /// Intake ceiling for async ingest — see
    /// [`DEFAULT_ENCODE_QUEUE_MAX`]. Compared against
    /// `objects_encoding.load()` at the top of
    /// `ingest_bytes_async`; over the ceiling we return
    /// `AsyncQueueFull` (503+Retry-After) so the queue can't grow
    /// past a bounded RAM footprint.
    pub(crate) encode_queue_max: usize,
    /// TTL cache for `api_stats`. `api_stats` is O(N × M) — N
    /// manifests × M shard hashes — and must clone the whole
    /// catalog to avoid holding the mutex during the walk. Under
    /// a 50-worker soak that clone + walk combined with concurrent
    /// encoder finalises + `persist_catalog` fsyncs contended the
    /// same mutex for seconds at a time, tripping the 10 s SHORT
    /// bucket timeout for 20 % of `/api/stats` calls. Cache the
    /// result for 500 ms — soak/dashboard callers don't need
    /// sub-second freshness, and the mutex churn drops to at most
    /// two contended acquisitions per second.
    pub(crate) stats_cache: Arc<tokio::sync::Mutex<Option<(std::time::Instant, ApiStats)>>>,
    /// Group-commit coalescing for [`Self::persist_catalog`]. Every
    /// mutation increments `persist_dirty_epoch` after it commits;
    /// the flush leader (single-writer serialised on
    /// `persist_flush_mutex`) atomically snapshots the catalog +
    /// the current dirty epoch, writes, then publishes to
    /// `persist_flushed_epoch`. Concurrent callers whose ticket is
    /// already covered by an in-flight or completed flush skip the
    /// fsync entirely and count in `persist_coalesced_total`.
    /// Under the July 2026 soak this cut per-mutation persist
    /// latency from ~100 ms (fsync serialisation) to near-zero for
    /// followers.
    pub(crate) persist_dirty_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) persist_flushed_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) persist_flush_mutex: Arc<tokio::sync::Mutex<()>>,
    pub(crate) persist_coalesced_total: Arc<std::sync::atomic::AtomicU64>,
}

/// PNG cache entry: fully-encoded body + the layer it was decoded at
/// + the accounting metadata the response headers echo back. Lives
/// under [`Gateway::cache`] keyed by `(name, max_layer)`; entries are
/// dropped by [`Gateway::invalidate_cache`] on PUT / DELETE.
pub(crate) struct CachedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) max_layer: u8,
    pub(crate) bytes_downloaded: u64,
    pub(crate) decode_ms: u128,
}

impl Gateway {
    /// Construct an in-memory Gateway (catalog not persisted to disk).
    pub fn new(
        gf: Arc<Gf>,
        catalog: Arc<RwLock<Directory>>,
        live: Arc<LiveNodes>,
        cluster: Arc<ClusterInfo>,
    ) -> Arc<Self> {
        let n = cluster.node_addrs.len();
        Arc::new(Self {
            catalog,
            catalog_path: None,
            embed: Arc::new(Mutex::new(EmbedState::default())),
            versions: Arc::new(Mutex::new(VersionsState::default())),
            gc_barrier: Arc::new(RwLock::new(())),
            gf,
            live,
            admin_kills: Arc::new(Mutex::new(vec![false; n])),
            cluster,
            cache: Mutex::new(HashMap::new()),
            shard_cache: Mutex::new(HashMap::new()),
            escrow_cache: Mutex::new(HashMap::new()),
            fingerprint_cache: Mutex::new(HashMap::new()),
            auto_repair_cooldown: Mutex::new(HashMap::new()),
            auto_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_repair_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_runs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            catalog_persist_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            medium_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MEDIUM_CONCURRENCY)),
            long_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_LONG_CONCURRENCY)),
            medium_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            long_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_short_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_medium_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_long_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_monitor: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_auditor: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_scrub: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_missing_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_bad_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_disabled_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rate_limit_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            objects_encoding: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_completed_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_failed_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_ENCODE_CONCURRENCY)),
            encode_queue_max: DEFAULT_ENCODE_QUEUE_MAX,
            persist_dirty_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            persist_flushed_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            persist_flush_mutex: Arc::new(tokio::sync::Mutex::new(())),
            persist_coalesced_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stats_cache: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Same as [`Self::new`] but with a catalog file path: every PUT/DELETE
    /// persists atomically via [`Self::persist_catalog`].
    pub fn new_persistent(
        gf: Arc<Gf>,
        catalog: Arc<RwLock<Directory>>,
        live: Arc<LiveNodes>,
        cluster: Arc<ClusterInfo>,
        catalog_path: std::path::PathBuf,
    ) -> Arc<Self> {
        let n = cluster.node_addrs.len();
        Arc::new(Self {
            catalog,
            catalog_path: Some(catalog_path),
            embed: Arc::new(Mutex::new(EmbedState::default())),
            versions: Arc::new(Mutex::new(VersionsState::default())),
            gc_barrier: Arc::new(RwLock::new(())),
            gf,
            live,
            admin_kills: Arc::new(Mutex::new(vec![false; n])),
            cluster,
            cache: Mutex::new(HashMap::new()),
            shard_cache: Mutex::new(HashMap::new()),
            escrow_cache: Mutex::new(HashMap::new()),
            fingerprint_cache: Mutex::new(HashMap::new()),
            auto_repair_cooldown: Mutex::new(HashMap::new()),
            auto_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_repair_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_runs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            catalog_persist_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            medium_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MEDIUM_CONCURRENCY)),
            long_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_LONG_CONCURRENCY)),
            medium_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            long_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_short_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_medium_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout_long_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_monitor: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_auditor: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            task_restarts_scrub: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_missing_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_bad_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admin_auth_disabled_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rate_limit_rejected_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            objects_encoding: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_completed_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_failed_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encode_permits: Arc::new(tokio::sync::Semaphore::new(DEFAULT_ENCODE_CONCURRENCY)),
            encode_queue_max: DEFAULT_ENCODE_QUEUE_MAX,
            persist_dirty_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            persist_flushed_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            persist_flush_mutex: Arc::new(tokio::sync::Mutex::new(())),
            persist_coalesced_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stats_cache: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// N3: reset the backpressure permits to `medium` / `long`
    /// capacity. Called once at bootstrap after reading env vars.
    /// Uses `Semaphore::new(cap)` to make a fresh semaphore; the old
    /// one is dropped. Safe to call before axum starts serving —
    /// afterwards would race with in-flight `try_acquire_owned`
    /// calls and lose their permits.
    pub fn configure_limits(&mut self, medium: usize, long: usize) {
        self.medium_permits = Arc::new(tokio::sync::Semaphore::new(medium));
        self.long_permits = Arc::new(tokio::sync::Semaphore::new(long));
    }

    /// N3: reset the async-ingest encode-worker concurrency cap.
    /// Called once at bootstrap after reading `HOLOFS_ENCODE_CONCURRENCY`.
    /// Same lifecycle constraint as [`Self::configure_limits`]: safe
    /// only before axum starts serving.
    pub fn configure_encode_limit(&mut self, encode: usize) {
        self.encode_permits = Arc::new(tokio::sync::Semaphore::new(encode));
    }

    /// Set the async-ingest intake ceiling — see
    /// [`DEFAULT_ENCODE_QUEUE_MAX`]. Bootstrap reads
    /// `HOLOFS_ENCODE_QUEUE_MAX` and applies it once before axum
    /// begins serving.
    pub fn configure_encode_queue_max(&mut self, queue_max: usize) {
        self.encode_queue_max = queue_max;
    }

    /// Handle for the /metrics endpoint (backpressure permits +
    /// timeout + task-restart counters). Everything the middleware
    /// stack in `holofs-web` mutates is reachable via the returned
    /// bundle without leaking the private field names.
    pub fn observability_counters(&self) -> ObservabilityCounters<'_> {
        ObservabilityCounters {
            medium_permits: &self.medium_permits,
            long_permits: &self.long_permits,
            medium_rejected_total: &self.medium_rejected_total,
            long_rejected_total: &self.long_rejected_total,
            timeout_short_total: &self.timeout_short_total,
            timeout_medium_total: &self.timeout_medium_total,
            timeout_long_total: &self.timeout_long_total,
            task_restarts_monitor: &self.task_restarts_monitor,
            task_restarts_auditor: &self.task_restarts_auditor,
            task_restarts_scrub: &self.task_restarts_scrub,
            catalog_persist_failures_total: &self.catalog_persist_failures_total,
            admin_auth_missing_total: &self.admin_auth_missing_total,
            admin_auth_bad_total: &self.admin_auth_bad_total,
            admin_auth_disabled_total: &self.admin_auth_disabled_total,
            rate_limit_rejected_total: &self.rate_limit_rejected_total,
            objects_encoding: &self.objects_encoding,
            encode_completed_total: &self.encode_completed_total,
            encode_failed_total: &self.encode_failed_total,
        }
    }

    /// Owned handle to the per-IP rate-limit rejection counter.
    /// `holofs-web`'s middleware captures this when building its
    /// `RateLimit` config so both the middleware and `/metrics`
    /// see the same atomic.
    pub fn rate_limit_rejected_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.rate_limit_rejected_total)
    }

    /// Owned handles to the three admin-auth failure counters, in
    /// `(missing, bad, disabled)` order. Middleware in `holofs-web`
    /// captures these into its `from_fn` closure.
    pub fn admin_auth_counters(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        (
            Arc::clone(&self.admin_auth_missing_total),
            Arc::clone(&self.admin_auth_bad_total),
            Arc::clone(&self.admin_auth_disabled_total),
        )
    }

    /// Owned handles for a specific bucket's permit + rejection
    /// counter. `holofs-web`'s middleware captures the returned
    /// pair into a `from_fn` closure to enforce backpressure per
    /// bucket.
    pub fn medium_bucket(&self) -> (Arc<tokio::sync::Semaphore>, Arc<std::sync::atomic::AtomicU64>) {
        (Arc::clone(&self.medium_permits), Arc::clone(&self.medium_rejected_total))
    }

    /// See [`Self::medium_bucket`].
    pub fn long_bucket(&self) -> (Arc<tokio::sync::Semaphore>, Arc<std::sync::atomic::AtomicU64>) {
        (Arc::clone(&self.long_permits), Arc::clone(&self.long_rejected_total))
    }

    /// Owned handles to the per-bucket timeout counters, in
    /// `(short, medium, long)` order. Middleware picks whichever
    /// matches its bucket.
    pub fn timeout_counters(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        (
            Arc::clone(&self.timeout_short_total),
            Arc::clone(&self.timeout_medium_total),
            Arc::clone(&self.timeout_long_total),
        )
    }

    /// Owned handles to the three supervised-task restart counters,
    /// in `(monitor, auditor, scrub)` order. Bootstrap passes each
    /// one into the matching `supervised_spawn` call.
    pub fn task_restart_counters(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        (
            Arc::clone(&self.task_restarts_monitor),
            Arc::clone(&self.task_restarts_auditor),
            Arc::clone(&self.task_restarts_scrub),
        )
    }

    /// turn on the CLIP-based semantic-search index. Pass
    /// the on-disk path for the append-only `embeddings.bin` file.
    /// Idempotent; calling twice with different paths swaps the active
    /// index (any pre-existing embedder handle is dropped).
    pub async fn enable_embed(&self, index_path: std::path::PathBuf) {
        let mut s = self.embed.lock().await;
        s.enabled = true;
        s.index_path = Some(index_path);
        s.embedder = None;
    }

    /// Whether the embeddings index is wired up. Cheap — just reads the
    /// state mutex.
    pub async fn embed_enabled(&self) -> bool {
        self.embed.lock().await.enabled
    }

    /// turn on per-object version history. `dir` is the
    /// root under which `versions/<sanitized_name>/v…bin` files are
    /// written; we create it lazily on the first versioned PUT.
    pub async fn enable_versions(&self, dir: std::path::PathBuf) {
        let mut s = self.versions.lock().await;
        s.enabled = true;
        s.root = Some(dir);
    }

    /// Cap the per-object version history at `n` archived versions.
    /// When more than `n` versions exist for a name, the oldest ones
    /// are pruned (shards GC'd) at the next `archive_version` call.
    /// `n = 0` disables the cap (unlimited history).
    pub async fn set_versions_keep_last(&self, n: usize) {
        let mut s = self.versions.lock().await;
        s.keep_last = if n == 0 { None } else { Some(n) };
    }

    /// Whether version history is wired up.
    pub async fn versions_enabled(&self) -> bool {
        self.versions.lock().await.enabled
    }

    /// Snapshot of admin_kills flags (for reads outside Gateway, e.g. auditor).
    pub fn admin_kills_handle(&self) -> Arc<Mutex<Vec<bool>>> {
        Arc::clone(&self.admin_kills)
    }

    /// Shared catalog handle. Lock to read or mutate the directory.
    pub fn catalog(&self) -> &Arc<RwLock<Directory>> {
        &self.catalog
    }

    /// Shared cluster topology (node addresses, zones, placement scheme).
    pub fn cluster(&self) -> &Arc<ClusterInfo> {
        &self.cluster
    }

    /// Baseline live-node list (admin overrides applied on top of it).
    pub fn live(&self) -> &Arc<LiveNodes> {
        &self.live
    }

    /// Shared `Gf` instance — handlers reuse it instead of constructing a new one.
    pub fn gf(&self) -> &Arc<Gf> {
        &self.gf
    }

    /// Current list of "live" nodes taking admin override into account. Used
    /// by every read/write path inside Gateway, and by the new axum handlers
    /// in `holofs-web`.
    pub async fn effective_live(&self) -> LiveNodes {
        let kills = self.admin_kills.lock().await;
        self.live
            .iter()
            .copied()
            .filter(|&n| !kills.get(n).copied().unwrap_or(false))
            .collect()
    }

    /// Atomically persist the catalog to disk. `Ok(())` on success or
    /// when Gateway was built without a catalog path (`new` instead of
    /// `new_persistent`) — no-op is treated as success. On IO error
    /// this now bumps `catalog_persist_failures_total` and surfaces
    /// `GatewayError::Persist` to the caller (N4). Every writer path
    /// (ingest / mkdir / rmdir / rename / remove / restore /
    /// delete_version / repair_object_inplace) must propagate the
    /// Err so operators get an immediate 500 rather than a silent
    /// disk-full incident that a restart later exposes as lost data.
    pub async fn persist_catalog(&self) -> Result<(), crate::error::GatewayError> {
        use std::sync::atomic::Ordering;
        let Some(path) = &self.catalog_path else {
            return Ok(());
        };
        // Ticket: bump ONCE per caller — represents "there is a
        // mutation at least as recent as ticket N that needs to
        // reach disk". Callers get their ticket AFTER their catalog
        // mutation commits (they call persist_catalog last), so any
        // ticket ≤ current dirty_epoch is guaranteed observable in
        // the catalog at the moment we hold `persist_flush_mutex`.
        let my_ticket = self.persist_dirty_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        // Fast path: an earlier flush already covers our mutation.
        if self.persist_flushed_epoch.load(Ordering::Acquire) >= my_ticket {
            self.persist_coalesced_total.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Serialise the actual fsync. If N callers pile up here,
        // exactly one becomes leader and flushes the snapshot; the
        // rest re-check on entry and return.
        let _flush_guard = self.persist_flush_mutex.lock().await;

        if self.persist_flushed_epoch.load(Ordering::Acquire) >= my_ticket {
            self.persist_coalesced_total.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Leader path: take the catalog snapshot AFTER acquiring the
        // flush mutex so any mutation whose ticket landed before us
        // is guaranteed to be in the snapshot. Read dirty_epoch
        // WHILE holding the catalog lock — any concurrent mutation
        // is blocked on catalog.lock, so the number we read matches
        // the snapshot exactly. Publish that value to
        // flushed_epoch after the write succeeds; followers waiting
        // on the mutex see their ticket covered.
        let (snapshot, flush_epoch) = {
            let cat = self.catalog.read().await;
            let ep = self.persist_dirty_epoch.load(Ordering::Acquire);
            (cat.clone(), ep)
        };

        if let Err(e) = snapshot.save_atomic(path) {
            self.catalog_persist_failures_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                path = %path.display(),
                "catalog persist failed"
            );
            return Err(crate::error::GatewayError::Persist(format!(
                "save {}: {e}",
                path.display()
            )));
        }

        self.persist_flushed_epoch
            .store(flush_epoch, Ordering::Release);
        Ok(())
    }

    /// Drop cached decoded objects for `name` (every layer variant). Called
    /// after a successful PUT or DELETE so a subsequent GET re-decodes from
    /// the new shard layout. also drops the per-layer
    /// shard-payload cache so the next inspect render pulls fresh shards.
    pub async fn invalidate_cache(&self, name: &str) {
        let mut cache = self.cache.lock().await;
        cache.retain(|(n, _), _| n != name);
        drop(cache);
        let mut sc = self.shard_cache.lock().await;
        sc.retain(|(n, _, _), _| n != name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holofs_model::placement::Placement;
    use std::sync::atomic::Ordering;

    fn make_gw(catalog_path: Option<std::path::PathBuf>) -> Arc<Gateway> {
        let gf = Arc::new(Gf::new());
        let cat = Arc::new(RwLock::new(Directory::default()));
        let cluster = Arc::new(ClusterInfo {
            node_addrs: vec!["127.0.0.1:9999".into()],
            zones: vec![0],
            placement: Placement::Rendezvous,
            width: 8,
            height: 8,
        });
        let live: Vec<usize> = vec![0];
        match catalog_path {
            Some(p) => Gateway::new_persistent(gf, cat, Arc::new(live), cluster, p),
            None => Gateway::new(gf, cat, Arc::new(live), cluster),
        }
    }

    #[tokio::test]
    async fn persist_catalog_no_path_is_ok() {
        let gw = make_gw(None);
        // No catalog_path → success, no counter bump.
        assert!(gw.persist_catalog().await.is_ok());
        assert_eq!(
            gw.catalog_persist_failures_total.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn persist_catalog_reports_io_error() {
        // Point at a path whose parent doesn't exist so save_atomic
        // fails deterministically without needing a real disk-full.
        let bad = std::path::PathBuf::from("/nonexistent/holofs-persist-test-XYZ/catalog.bin");
        let gw = make_gw(Some(bad));
        let err = gw.persist_catalog().await.unwrap_err();
        match err {
            crate::error::GatewayError::Persist(msg) => {
                assert!(msg.contains("catalog.bin"), "msg was {msg:?}");
            }
            other => panic!("expected Persist, got {other:?}"),
        }
        assert_eq!(
            gw.catalog_persist_failures_total.load(Ordering::Relaxed),
            1,
            "counter should have incremented once"
        );
    }
}
