//! Holofs gateway: shared catalog + cluster state plus the data-plane methods
//! the axum frontend in [`holofs-web`] dispatches against.
//!
//! The hand-rolled HTTP/1.1 server (Stage 1–10 of the prototype) lived here
//! until Phase 4 of the migration. Everything HTTP-specific now lives in
//! `holofs-web`; this crate provides the [`Gateway`] type, its constructors
//! and accessors, and the `pub async fn` methods that take owned bytes /
//! strings and return view-model structs the frontend can render.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use holofs_client::LiveNodes;
use holofs_core::gf::Gf;
use holofs_model::fs::Directory;
use holofs_model::manifest::ObjectKind;
use holofs_model::placement::Placement;

/// Cluster metadata needed to PUT a new object.
pub struct ClusterInfo {
    pub node_addrs: Vec<String>,
    pub zones: Vec<u8>,
    pub placement: Placement,
    pub width: usize,
    pub height: usize,
}

pub struct Gateway {
    // Every field is `pub(crate)` because Phase R1b is splitting
    // the impl Gateway blocks across sibling modules
    // (search.rs, versions.rs, gc.rs, ...) — each of which reaches
    // into shared state.  The Gateway type itself stays `pub`; the
    // fields don't leak outside the crate boundary.
    pub(crate) catalog: Arc<Mutex<Directory>>,
    /// Optional path to the catalog file. If set, the catalog is saved
    /// atomically on each change (PUT/DELETE).
    pub(crate) catalog_path: Option<std::path::PathBuf>,
    /// Stage 12.8: optional semantic-search embeddings index. `None`
    /// when the server was started without `--enable-embed`. When set,
    /// every PUT fires a fire-and-forget background task that embeds
    /// the new object via CLIP and appends to the on-disk index.
    pub(crate) embed: Arc<Mutex<EmbedState>>,
    /// Stage 13.4: optional per-object version history. When enabled
    /// every PUT that *replaces* an existing object writes the prior
    /// manifest as a side file under `versions_dir/<sanitized>/v…bin`
    /// and skips the usual shard purge so the historical version
    /// remains decodeable. Trade-off: cluster storage monotonically
    /// grows while the feature is on (no GC yet).
    pub(crate) versions: Arc<Mutex<VersionsState>>,
    /// Stage 14.4: serialisation barrier between catalog-mutating
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
    /// Stage 11.2: per-`(name, channel, layer)` shard cache. `shard_payload`
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
    /// Auto-repair-on-read counters. Bumped from
    /// [`Self::decode_with_autorepair`] when the first decode attempt
    /// hits [`ClientError::LayerLost`] and the retry path kicks in.
    /// Surfaced via [`ApiStats`].
    pub(crate) auto_repairs_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) auto_repair_failures_total: Arc<std::sync::atomic::AtomicU64>,
    /// Background-scrub counters: how many objects this gateway has
    /// proactively repaired before any user GET tripped a 503.
    /// Bumped from the scrub task spawned at bootstrap (see
    /// [`Self::scrub_tick`]).
    pub(crate) scrub_repairs_total: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) scrub_runs_total: Arc<std::sync::atomic::AtomicU64>,
}

pub(crate) struct CachedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) max_layer: u8,
    pub(crate) bytes_downloaded: u64,
    pub(crate) decode_ms: u128,
}

impl Gateway {
    pub fn new(
        gf: Arc<Gf>,
        catalog: Arc<Mutex<Directory>>,
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
            auto_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_repair_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_runs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Same as `new`, but with a catalog file path: every PUT/DELETE persists.
    pub fn new_persistent(
        gf: Arc<Gf>,
        catalog: Arc<Mutex<Directory>>,
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
            auto_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_repair_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_repairs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scrub_runs_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Stage 12.8: turn on the CLIP-based semantic-search index. Pass
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

    /// Stage 13.4: turn on per-object version history. `dir` is the
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
    pub fn catalog(&self) -> &Arc<Mutex<Directory>> {
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

    // `scrub_tick` moved to `health.rs` in Phase R1b.12.

    // `decode_with_autorepair`, `repair_object_inplace`, `purge_orphans_of`
    // moved to `repair.rs` in Phase R1b.13.

    /// Atomically persist the catalog to disk. No-op if Gateway was built
    /// without a catalog path (`new` instead of `new_persistent`).
    pub async fn persist_catalog(&self) {
        let Some(path) = &self.catalog_path else {
            return;
        };
        // Snapshot: avoid holding the Mutex across fsync.
        let snapshot = self.catalog.lock().await.clone();
        if let Err(e) = snapshot.save_atomic(path) {
            eprintln!("failed to save catalog to {path:?}: {e}");
        }
    }

    /// Drop cached decoded objects for `name` (every layer variant). Called
    /// after a successful PUT or DELETE so a subsequent GET re-decodes from
    /// the new shard layout. Stage 11.2: also drops the per-layer
    /// shard-payload cache so the next inspect render pulls fresh shards.
    pub async fn invalidate_cache(&self, name: &str) {
        let mut cache = self.cache.lock().await;
        cache.retain(|(n, _), _| n != name);
        drop(cache);
        let mut sc = self.shard_cache.lock().await;
        sc.retain(|(n, _, _), _| n != name);
    }


    // `compute_fingerprint` moved to `fingerprint.rs` in Phase R1b.15.

    // put_any + blank_manifest / blank_audio_manifest / blank_text_manifest
    // / blank_opaque_manifest + ingest_bytes + IngestResult moved to
    // `ingest.rs` in Phase R1b.16.

    // get_or_decode moved to `decode.rs` in Phase R1b.17.
}

// === Public API for external HTTP frontends (e.g. holofs-web axum handlers) ===

// DecodedObject moved to `decode.rs` in Phase R1b.17.

// IngestResult moved to `ingest.rs` in Phase R1b.16.

// RemoveResult / MkdirResult / RmdirResult / RenameResult moved to
// `dirops.rs` in Phase R1b.11.

/// Current Unix epoch seconds. Stamped onto every PUT'd manifest and
/// every newly created `Directory` marker so the catalog can be sorted
/// by creation time later. Falls back to `0` if the clock is somehow
/// behind the epoch (we don't want to panic the whole ingest path on
/// what should be impossible).
// GatewayError, helpers — extracted to sibling modules
// (`error.rs`, `util.rs`) in Phase R1b.1. The re-exports on lib.rs
// preserve the public path.
use crate::error::GatewayError;
use crate::search::EmbedState;
use crate::versions::VersionsState;

impl Gateway {
    // decode_object moved to `decode.rs` in Phase R1b.17.
    // ingest_bytes moved to `ingest.rs` in Phase R1b.16.

    // remove_object / mkdir / rmdir / rename / list_dir moved to
    // `dirops.rs` in Phase R1b.11.

    // api_stats / health_index_data / object_health / toggle_admin_kill
    // moved to `health.rs` in Phase R1b.12.

    // `fingerprint_of`, `compute_fingerprint_for`, `similar_to`, and
    // FingerprintInfo moved to `fingerprint.rs` in Phase R1b.15.

    // inspect + shard_payload moved to `inspect.rs` in Phase R1b.8.
    // `diff_chunks` + Diff{Cell,Layer,Report} moved to `diff.rs` in R1b.10.
    // `mix_objects` + `filter_audio` moved to `mix.rs` in Phase R1b.9.
    // `file_metrics` + FileMetrics / NeighbourMetric / AudioBandEnergy
    // moved to `metrics.rs` in Phase R1b.14.
}

// === Stage 12.8 CLIP-based semantic search ==================================
//
// SearchBand / SemanticHit / EmbedState / semantic_search /
// embed_object / embed_object_in_background moved to `search.rs`
// in Phase R1b.2. The public path stays: SearchBand + SemanticHit
// are re-exported at the crate root via `lib.rs`.

impl Gateway {

    // spotlight + spotlight_coeff moved to `spotlight.rs`
    // in Phase R1b.7. Public types SpotlightRoi + SpotlightImage
    // re-exported at the crate root.


    /// Walk the catalog and embed every image that isn't in the index
    /// yet. Returns `(newly_embedded, skipped)`. Used by the
    /// `holofs embed-all` CLI command.
    pub async fn embed_all_pending(&self) -> Result<(usize, usize), GatewayError> {
        let names: Vec<String> = {
            let cat = self.catalog.lock().await;
            cat.entries
                .iter()
                .filter_map(|(name, m)| {
                    if m.kind == ObjectKind::Image {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let mut new_n = 0;
        let mut skip_n = 0;
        for name in names {
            match self.embed_object(&name).await {
                Ok(true) => new_n += 1,
                Ok(false) => skip_n += 1,
                Err(e) => {
                    // Continue on per-file failure — one broken object
                    // shouldn't stall the whole catalog index.
                    eprintln!("embed {name}: {e}");
                }
            }
        }
        Ok((new_n, skip_n))
    }
}

// === Stage 13.4 per-object version history =============================
//
// VersionsState + VersionEntry + RestoreResult + DeleteVersionResult
// and every version-related impl Gateway method moved to `versions.rs`
// in Phase R1b.3. Public types re-exported at the crate root via lib.rs.


// === Stage 14.0 orphan-shard garbage collection ========================
//
// GcNodeReport + GcReport + gc_orphaned_shards moved to `gc.rs`
// in Phase R1b.4. Public types re-exported at the crate root.


// KindCounts / ApiStats / ScrubReport moved to `health.rs` in Phase R1b.12.

// FingerprintInfo moved to `fingerprint.rs` in Phase R1b.15.

// NodeStatus / HealthIndexData / AdminToggleResult moved to `health.rs`
// in Phase R1b.12.


// === Similarity types + scope helpers ==============================
//
// SimilarScope, SimilarityMethod, SimilarMatch, ShardOverlap,
// SimilarReport, parent_dir, in_scope moved to `similarity.rs`
// in Phase R1b.6. The `similar_to` method itself still lives
// in this file (shares its impl block with chunk_diff).
// Public types re-exported at the crate root.


// Diff{Cell,Layer,Report} moved to `diff.rs` in Phase R1b.10.
// `MixedImage` + `FilteredAudio` moved to `mix.rs` in Phase R1b.9.

// NeighbourMetric / FileMetrics / AudioBandEnergy moved to `metrics.rs`
// in Phase R1b.14.

// === Stage 8 holographic key escrow ==================================
//
// EscrowShareInfo + EscrowSplitResult + EscrowShareBytes +
// EscrowRecoverResult and the three escrow impl Gateway methods
// moved to `escrow.rs` in Phase R1b.5.


// guess_opaque_content_type, guess_text_content_type, encode_png —
// moved to `util.rs` in Phase R1b.1.
// directory_object_id test lives with the fn in util.rs.
// parent_dir / in_scope / SimilarScope::parse tests moved to
// `similarity.rs` alongside the code they exercise (Phase R1b.15).
