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
use std::time::Instant;

use tokio::sync::{Mutex, RwLock};

use holofs_client::{
    get_object_up_to_layer, get_object_with_coeff_mask, layer_energies, put_object, repair_node,
    ClientError, LiveNodes,
};
use holofs_codec::image_io::load_photo_from_bytes;
use holofs_core::gf::Gf;
use holofs_core::hash::hex;
use holofs_core::transform::coeff_layer;
use holofs_core::{K, LEVELS, NLAYERS, RED};
use holofs_model::fs::Directory;
use holofs_model::manifest::{Manifest, ObjectKind};
use holofs_model::path as catalog_path;
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


    /// Compute the object's perceptual fingerprint. Fetches shards (channel=0,
    /// layer=0) from live nodes, filters by hash, then calls
    /// [`holofs_analytics::fingerprint::perceptual_fingerprint`]. For opaque/text
    /// the fingerprint degenerates to the first bytes of the CID.
    async fn compute_fingerprint(
        &self,
        manifest: &Manifest,
    ) -> holofs_analytics::fingerprint::Fingerprint {
        use holofs_model::manifest::ObjectKind;
        if matches!(manifest.kind, ObjectKind::Opaque | ObjectKind::Text) {
            return holofs_analytics::fingerprint::perceptual_fingerprint(manifest, &[]);
        }
        let live = self.effective_live().await;
        // Stage 11.6: gather L0 for every channel (up to 3 — fingerprint
        // covers RGB). Per-channel means power the new 45-bit dHash; a
        // single-channel fingerprint clustered too many unrelated images
        // at 95%+ similarity. The per-(name, channel, layer) cache (Stage
        // 11.2) deduplicates concurrent gathers for the same object.
        let n_channels = (manifest.channels as usize).min(3);
        let mut per_channel: Vec<Vec<holofs_core::rlnc::Shard>> =
            Vec::with_capacity(n_channels);
        for c in 0..n_channels {
            let shards = holofs_client::gather_layer(manifest, &live, c as u8, 0)
                .await
                .unwrap_or_default();
            let expected: std::collections::HashSet<_> = manifest
                .shard_hashes
                .get(c)
                .and_then(|cl| cl.first())
                .map(|hs| hs.iter().copied().collect())
                .unwrap_or_default();
            let verified: Vec<holofs_core::rlnc::Shard> = shards
                .into_iter()
                .filter(|s| expected.contains(&holofs_core::merkle::shard_hash(s)))
                .collect();
            per_channel.push(verified);
        }
        holofs_analytics::fingerprint::perceptual_fingerprint(manifest, &per_channel)
    }

    /// Universal PUT: tries image → audio → text → reject. Returns the
    /// finished Manifest (with shards already distributed), the kind label,
    /// and the total shard count.
    async fn put_any(
        &self,
        name: &str,
        body: &[u8],
        live: &LiveNodes,
    ) -> Result<(Manifest, &'static str, u32), String> {
        // 1. Image — the most common case, try first.
        if let Ok(arr) = load_photo_from_bytes(body, self.cluster.width, self.cluster.height) {
            let channels = vec![arr[0].clone(), arr[1].clone(), arr[2].clone()];
            let mut m = self.blank_manifest();
            put_object(&self.gf, &mut m, live, &channels)
                .await
                .map_err(|e| format!("put image: {e}"))?;
            let total: u32 = m.n_per_layer.iter().sum::<u32>() * m.channels as u32;
            return Ok((m, "image", total));
        }
        // 2. Audio — let symphonia try to recognize the format.
        if let Ok(audio) = holofs_codec::audio_codec::decode_audio_from_bytes(body) {
            let mut m = self
                .blank_audio_manifest(
                    audio.sample_rate,
                    audio.n_channels(),
                    audio.sample_count as usize,
                )
                .map_err(|e| format!("audio manifest: {e}"))?;
            holofs_client::put_audio_object(&self.gf, &mut m, live, &audio.channels)
                .await
                .map_err(|e| format!("put audio: {e}"))?;
            let total: u32 = m.n_per_layer.iter().sum::<u32>() * m.channels as u32;
            return Ok((m, "audio", total));
        }
        // 3. Text — UTF-8 validation. If it fails → opaque.
        if let Ok(text) = std::str::from_utf8(body) {
            let ct = guess_text_content_type(name);
            let mut m = self.blank_text_manifest(ct);
            holofs_client::put_text_object(&self.gf, &mut m, live, text)
                .await
                .map_err(|e| format!("put text: {e}"))?;
            let total = m.n_per_layer[0];
            return Ok((m, "text", total));
        }
        // 4. Opaque blob — last fallback. PDF, DOCX, ZIP, EXE, etc.
        let ct = guess_opaque_content_type(name);
        let mut m = self.blank_opaque_manifest(ct);
        holofs_client::put_opaque_object(&self.gf, &mut m, live, body)
            .await
            .map_err(|e| format!("put opaque: {e}"))?;
        let total = m.n_per_layer[0];
        Ok((m, "opaque", total))
    }

    /// Blank manifest for an opaque object: 1 channel, 1 layer, n shards = K + K/2
    /// (×1.5 margin, same as text). sym_len is filled by put_opaque_object.
    fn blank_opaque_manifest(&self, content_type: String) -> Manifest {
        let n_shards = K + K / 2;
        Manifest {
            object_id: 0,
            k: K as u16,
            nlayers: 1,
            n_per_layer: vec![n_shards as u32],
            sym_len: vec![0],
            layer_positions: vec![vec![]],
            channels: 1,
            width: 0,
            height: 0,
            levels: 0,
            nodes: self.cluster.node_addrs.clone(),
            placement: self.cluster.placement,
            zones: self.cluster.zones.clone(),
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: holofs_model::manifest::ObjectKind::Opaque,
            content_type,
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
        }
    }

    /// Blank manifest for an audio object. `width = sample_count`, `height = 1`,
    /// `channels = 1 or 2`. 1D DWT, same 4 layer priorities as for image.
    fn blank_audio_manifest(
        &self,
        sample_rate: u32,
        channels: u8,
        sample_count: usize,
    ) -> Result<Manifest, String> {
        // Round sample_count down to a multiple of 2^LEVELS for DWT.
        let align = 1usize << LEVELS;
        let n_samples = (sample_count / align) * align;
        if n_samples == 0 {
            return Err(format!(
                "audio too short: {sample_count} samples < {align}"
            ));
        }
        let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for i in 0..n_samples {
            layer_positions[holofs_core::transform::coeff_layer_1d(i, n_samples)].push(i as u32);
        }
        let n_per_layer: Vec<u32> = (0..NLAYERS)
            .map(|l| (K as f32 * RED[l]).round() as u32)
            .collect();
        let sym_len: Vec<u32> = layer_positions
            .iter()
            .map(|pos| ((pos.len() * 4 + K - 1) / K) as u32)
            .collect();
        Ok(Manifest {
            object_id: 0,
            k: K as u16,
            nlayers: NLAYERS as u8,
            n_per_layer,
            sym_len,
            layer_positions,
            channels,
            width: n_samples as u32,
            height: 1,
            levels: LEVELS as u8,
            nodes: self.cluster.node_addrs.clone(),
            placement: self.cluster.placement,
            zones: self.cluster.zones.clone(),
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); NLAYERS]; channels as usize],
            kind: holofs_model::manifest::ObjectKind::Audio,
            content_type: "audio/wav".into(),
            chunk_lens: vec![],
            audio_sample_rate: sample_rate,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
        })
    }

    /// Blank manifest for a text object: 1 channel, 1 layer, no DWT.
    /// `n_per_layer[0]` is the total shard count (systematic K + RLNC margin).
    /// We use the same redundancy as the rarest image layer (×1.15).
    fn blank_text_manifest(&self, content_type: String) -> Manifest {
        // shard count for text: K systematic + ceil(K * 0.5) RLNC = ×1.5 margin.
        let n_shards = K + K / 2;
        Manifest {
            object_id: 0,
            k: K as u16,
            nlayers: 1,
            n_per_layer: vec![n_shards as u32],
            sym_len: vec![0], // filled by put_text_object
            layer_positions: vec![vec![]],
            channels: 1,
            width: 0,
            height: 0,
            levels: 0,
            nodes: self.cluster.node_addrs.clone(),
            placement: self.cluster.placement,
            zones: self.cluster.zones.clone(),
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: holofs_model::manifest::ObjectKind::Text,
            content_type,
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
        }
    }

    fn blank_manifest(&self) -> Manifest {
        let w = self.cluster.width;
        let h = self.cluster.height;
        let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                layer_positions[coeff_layer(x, y, w, h)].push((y * w + x) as u32);
            }
        }
        let n_per_layer: Vec<u32> = (0..NLAYERS)
            .map(|l| (K as f32 * RED[l]).round() as u32)
            .collect();
        let sym_len: Vec<u32> = layer_positions
            .iter()
            .map(|pos| ((pos.len() * 4 + K - 1) / K) as u32)
            .collect();
        Manifest {
            object_id: 0,
            k: K as u16,
            nlayers: NLAYERS as u8,
            n_per_layer,
            sym_len,
            layer_positions,
            channels: 3,
            width: w as u32,
            height: h as u32,
            levels: LEVELS as u8,
            nodes: self.cluster.node_addrs.clone(),
            placement: self.cluster.placement,
            zones: self.cluster.zones.clone(),
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); NLAYERS]; 3],
            kind: holofs_model::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
        }
    }

    async fn get_or_decode(&self, name: &str, max_layer: u8) -> Option<Arc<CachedFile>> {
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&(name.to_string(), max_layer)) {
                return Some(Arc::clone(c));
            }
        }
        // The catalog snapshot + live set are taken inside
        // `decode_with_autorepair`; this branch only needs the
        // post-decode width/height (which doesn't change across
        // auto-repair since `repair_node` only rewrites
        // `shard_hashes`, not dimensions).
        let (width, height) = {
            let cat = self.catalog.lock().await;
            let m = cat.get(name)?;
            (m.width, m.height)
        };
        let t0 = Instant::now();
        let (channels, bytes) = self
            .decode_with_autorepair(name, max_layer)
            .await
            .ok()?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, width, height);
        let entry = Arc::new(CachedFile {
            bytes: png,
            max_layer,
            bytes_downloaded: bytes,
            decode_ms,
        });
        self.cache
            .lock()
            .await
            .insert((name.to_string(), max_layer), Arc::clone(&entry));
        Some(entry)
    }
}

// === Public API for external HTTP frontends (e.g. holofs-web axum handlers) ===

/// Decoded object payload ready to be wrapped into an HTTP response.
///
/// `bytes` is the body the client receives; `content_type` is the MIME the
/// frontend must echo. Everything else maps to `X-Holofs-*` headers or the
/// audio/text-specific headers (`Sample-Rate`, `Channels`, `Chunks-Total`,
/// `Chunks-Missing`).
#[derive(Debug, Clone)]
pub struct DecodedObject {
    /// Final encoded body (PNG, WAV, UTF-8 text, or raw opaque bytes).
    pub bytes: Vec<u8>,
    /// `Content-Type` to put on the response.
    pub content_type: String,
    /// Object kind, useful for `X-Holofs-Kind`.
    pub kind: holofs_model::manifest::ObjectKind,
    /// Last layer that was decoded (image/audio only). `None` for text/opaque.
    pub max_layer: Option<u8>,
    /// Bytes pulled from cluster nodes during decode.
    pub bytes_downloaded: u64,
    /// Wall-clock decode time in milliseconds.
    pub decode_ms: u128,
    /// Audio: sample rate. `None` for non-audio.
    pub sample_rate: Option<u32>,
    /// Audio: channel count (1 or 2). `None` for non-audio.
    pub channels: Option<u8>,
    /// Text: total chunk count.
    pub chunks_total: Option<usize>,
    /// Text: number of chunks replaced by hole markers.
    pub chunks_missing: Option<usize>,
    /// Opaque: original filename for `Content-Disposition: attachment`.
    pub filename_for_disposition: Option<String>,
}

/// Summary of a successful PUT — fed back to the client as JSON.
#[derive(Debug, Clone)]
pub struct IngestResult {
    /// Catalog name the object was stored under.
    pub name: String,
    /// 64-bit `object_id` (first 8 bytes of `data_cid`).
    pub object_id: u64,
    /// Full SHA-256 hex of the source data + parameters.
    pub data_cid_hex: String,
    /// Image: width in pixels. 0 for non-image.
    pub width: u32,
    /// Image: height in pixels. 0 for non-image.
    pub height: u32,
    /// Detected object kind.
    pub kind: holofs_model::manifest::ObjectKind,
    /// Total shards dispatched across the cluster.
    pub total_shards: u32,
    /// Ingestion wall-clock time in milliseconds.
    pub put_ms: u128,
}

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
use crate::similarity::{
    in_scope, parent_dir, ShardOverlap, SimilarMatch, SimilarReport, SimilarScope,
    SimilarityMethod,
};
use crate::util::{
    directory_object_id, encode_png, guess_opaque_content_type,
    guess_text_content_type, now_unix,
};
use crate::versions::VersionsState;

impl Gateway {
    /// Decode an object for HTTP transport. Returns body bytes plus the
    /// metadata the frontend needs to populate response headers.
    ///
    /// `max_layer = None` → full quality (last layer of the manifest).
    /// `max_layer = Some(0)` → preview (image LL band or audio bass).
    /// Preview is rejected with [`GatewayError::PreviewUnsupported`] for
    /// text and opaque objects.
    pub async fn decode_object(
        &self,
        name: &str,
        max_layer: Option<u8>,
    ) -> Result<DecodedObject, GatewayError> {
        use holofs_model::manifest::ObjectKind;

        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let kind = manifest.kind;

        // Preview is only meaningful for image/audio.
        if matches!(kind, ObjectKind::Text | ObjectKind::Opaque) && max_layer.is_some() {
            return Err(GatewayError::PreviewUnsupported);
        }

        match kind {
            // Directories carry no payload — never reach the decode pipeline.
            ObjectKind::Directory => Err(GatewayError::IsDirectory),
            ObjectKind::Image => {
                let layer = max_layer.unwrap_or_else(|| manifest.nlayers.saturating_sub(1));
                let entry = self
                    .get_or_decode(name, layer)
                    .await
                    .ok_or_else(|| GatewayError::Decode(format!("image decode failed: {name}")))?;
                Ok(DecodedObject {
                    bytes: entry.bytes.clone(),
                    content_type: "image/png".into(),
                    kind,
                    max_layer: Some(entry.max_layer),
                    bytes_downloaded: entry.bytes_downloaded,
                    decode_ms: entry.decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Audio => {
                let layer = max_layer.unwrap_or_else(|| manifest.nlayers.saturating_sub(1));
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let (channels, bytes_dl) = holofs_client::get_audio_object_up_to_layer(
                    &self.gf, &manifest, &live, layer,
                )
                .await
                .map_err(|e| GatewayError::Decode(format!("audio decode: {e}")))?;
                let decode_ms = t0.elapsed().as_millis();
                let wav = holofs_codec::audio_codec::encode_wav_16bit(
                    &channels,
                    manifest.audio_sample_rate,
                );
                Ok(DecodedObject {
                    bytes: wav,
                    content_type: "audio/wav".into(),
                    kind,
                    max_layer: Some(layer),
                    bytes_downloaded: bytes_dl,
                    decode_ms,
                    sample_rate: Some(manifest.audio_sample_rate),
                    channels: Some(manifest.channels),
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Text => {
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let (bytes, holes) =
                    holofs_client::get_text_object_with_holes(&self.gf, &manifest, &live)
                        .await
                        .map_err(|e| GatewayError::Decode(format!("text decode: {e}")))?;
                let decode_ms = t0.elapsed().as_millis();
                Ok(DecodedObject {
                    bytes,
                    content_type: manifest.content_type.clone(),
                    kind,
                    max_layer: None,
                    bytes_downloaded: 0,
                    decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: Some(manifest.chunk_lens.len()),
                    chunks_missing: Some(holes),
                    filename_for_disposition: None,
                })
            }
            ObjectKind::Opaque => {
                let live = self.effective_live().await;
                let t0 = Instant::now();
                let bytes = holofs_client::get_opaque_object(&self.gf, &manifest, &live)
                    .await
                    .map_err(|e| {
                        GatewayError::Decode(format!("opaque decode (need ≥K shards): {e}"))
                    })?;
                let decode_ms = t0.elapsed().as_millis();
                Ok(DecodedObject {
                    bytes,
                    content_type: manifest.content_type.clone(),
                    kind,
                    max_layer: None,
                    bytes_downloaded: 0,
                    decode_ms,
                    sample_rate: None,
                    channels: None,
                    chunks_total: None,
                    chunks_missing: None,
                    filename_for_disposition: Some(name.to_string()),
                })
            }
        }
    }

    /// Auto-detect kind and ingest bytes (image → audio → text → opaque).
    /// Replaces any existing object with the same name. Persists catalog
    /// and clears cache on success.
    pub async fn ingest_bytes(
        &self,
        name: &str,
        body: &[u8],
    ) -> Result<IngestResult, GatewayError> {
        // Stage 14.4: hold the GC barrier for the full PUT. GC
        // upgrades to a write guard and waits for us; any
        // concurrent GC blocks new PUTs until it's done. Released
        // automatically on function return.
        let _gc_guard = self.gc_barrier.read().await;
        if body.is_empty() {
            return Err(GatewayError::BadRequest("empty body".into()));
        }
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        // Reject before doing any expensive work: cannot write where a
        // directory already lives, and the parent directory must exist.
        {
            let cat = self.catalog.lock().await;
            if let Some(existing) = cat.get(name) {
                if existing.kind == ObjectKind::Directory {
                    return Err(GatewayError::AlreadyExists);
                }
            }
            if let Some(parent) = catalog_path::parent(name) {
                match cat.get(parent) {
                    Some(m) if m.kind == ObjectKind::Directory => {}
                    Some(_) => return Err(GatewayError::NotADirectory),
                    None => {
                        return Err(GatewayError::BadRequest(format!(
                            "parent directory does not exist: {parent}"
                        )))
                    }
                }
            }
        }
        let live = self.effective_live().await;
        // A fully-down cluster previously panicked inside `place_shard`
        // ("live node set is empty"). Now we surface a clean 503 so
        // clients can back off and retry without dragging the gateway
        // down with them.
        if live.is_empty() {
            return Err(GatewayError::ClusterDegraded);
        }
        let prev = self.catalog.lock().await.get(name).cloned();
        if let Some(old) = &prev {
            // Stage 13.4: when versioning is on we archive the prior
            // manifest as a side file AND skip the shard purge — the
            // old shards must stay live so a `restore_version` call
            // can decode them again. Trade-off: storage grows
            // monotonically until either a GC pass lands or the
            // operator drops the version side files manually.
            if self.versions_enabled().await {
                if let Err(e) = self.archive_version(name, old).await {
                    eprintln!("PUT {name}: version archive failed: {e}");
                }
            } else {
                // Stage 15.2 fix: the prior `purge_object(old, &live)`
                // call yanked entire (object_id, channel, layer) buckets
                // on every node, which destroyed the shards of any
                // other catalog entry that happened to share the same
                // `data_cid` (and therefore `object_id`) — e.g.,
                // PUT-replacing one of two dedupe-matched copies broke
                // the survivor. We now scope the purge to hashes
                // unique to `old`. Caller's catalog mutation has not
                // yet replaced `old` in the catalog at this point;
                // pass through `purge_orphans_of` which itself walks
                // the catalog *as it is now*. `old` isn't in the
                // catalog yet for this name, but the new manifest
                // also isn't — so `purge_orphans_of` will only see
                // OTHER entries and the residue is correct.
                if let Err(e) = self.purge_orphans_of(old, &live, Some(name)).await {
                    eprintln!("PUT {name}: previous object failed to purge (continuing): {e}");
                }
            }
        }
        let t0 = Instant::now();
        let (mut manifest, _kind_str, total_shards) = self
            .put_any(name, body, &live)
            .await
            .map_err(GatewayError::BadRequest)?;
        // Stage 11.12: stamp the manifest with creation time so the
        // tree view can sort by date.
        manifest.created_at_unix = now_unix();
        let put_ms = t0.elapsed().as_millis();
        let object_id = manifest.object_id;
        let cid_hex = hex(&manifest.data_cid);
        let kind = manifest.kind;
        let width = manifest.width;
        let height = manifest.height;
        self.catalog
            .lock()
            .await
            .insert(name.to_string(), manifest);
        self.invalidate_cache(name).await;
        self.persist_catalog().await;
        Ok(IngestResult {
            name: name.to_string(),
            object_id,
            data_cid_hex: cid_hex,
            width,
            height,
            kind,
            total_shards,
            put_ms,
        })
    }

    // remove_object / mkdir / rmdir / rename / list_dir moved to
    // `dirops.rs` in Phase R1b.11.

    // api_stats / health_index_data / object_health / toggle_admin_kill
    // moved to `health.rs` in Phase R1b.12.

    /// Perceptual fingerprint of an object for `GET /api/fingerprint/<name>`.
    /// Image/audio: 16-byte L1 hash from L0 systematic shards. Text/opaque:
    /// fallback to the first 16 bytes of `data_cid`.
    pub async fn fingerprint_of(&self, name: &str) -> Result<FingerprintInfo, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let fp = self.compute_fingerprint(&manifest).await;
        Ok(FingerprintInfo {
            name: name.to_string(),
            fingerprint_hex: holofs_analytics::fingerprint::fingerprint_hex(&fp),
            kind: manifest.kind,
        })
    }

    // === Phase 4b.5: inspect / similar / diff public API ===================

    // inspect + shard_payload moved to `inspect.rs`
    // in Phase R1b.8. Public view-model types re-exported
    // at the crate root.


    /// Compute the object's perceptual fingerprint. Exposed for the
    /// `/similar/<name>` view-model in `holofs-web`. Image/audio reach into the
    /// L0 systematic shards; text/opaque fall back to the first 16 bytes of
    /// `data_cid`.
    pub async fn compute_fingerprint_for(
        &self,
        manifest: &Manifest,
    ) -> holofs_analytics::fingerprint::Fingerprint {
        self.compute_fingerprint(manifest).await
    }

    /// `/similar/<name>` view-model: top-10 neighbours of the same kind +
    /// cross-object shard overlaps (any kind). `scope` constrains the
    /// candidate pool relative to the target's parent directory — `All`
    /// scans the whole catalog (current behavior), `Folder` keeps only
    /// direct siblings, `Tree` keeps the subtree rooted at the parent.
    pub async fn similar_to(
        &self,
        name: &str,
        scope: SimilarScope,
    ) -> Result<SimilarReport, GatewayError> {
        use holofs_model::manifest::ObjectKind;

        let snapshot = self.catalog.lock().await.clone();
        let manifest = snapshot
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let is_text = manifest.kind == ObjectKind::Text;
        let target_fp = if is_text {
            [0u8; holofs_analytics::fingerprint::FP_LEN]
        } else {
            self.compute_fingerprint(&manifest).await
        };
        let target_parent = parent_dir(name);

        let mut neighbors: Vec<SimilarMatch> = Vec::new();
        for n in snapshot.names() {
            if n == name {
                continue;
            }
            if !in_scope(target_parent, &n, scope) {
                continue;
            }
            let m = match snapshot.get(&n) {
                Some(m) => m,
                None => continue,
            };
            if m.kind != manifest.kind {
                continue;
            }
            let (sim, method) = if is_text {
                let j = holofs_analytics::shingle::jaccard_similarity(
                    &manifest.text_minhash,
                    &m.text_minhash,
                );
                (j * 100.0, SimilarityMethod::Jaccard)
            } else {
                let fp = self.compute_fingerprint(m).await;
                let sim =
                    holofs_analytics::fingerprint::fingerprint_similarity_pct(&target_fp, &fp);
                (sim, SimilarityMethod::DHash)
            };
            neighbors.push(SimilarMatch {
                name: n,
                similarity_pct: sim,
                method,
            });
        }
        neighbors.sort_by(|a, b| {
            b.similarity_pct
                .partial_cmp(&a.similarity_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        neighbors.truncate(10);

        let mut overlaps: Vec<ShardOverlap> = Vec::new();
        let total_a = manifest.shard_hashes.iter().flatten().flatten().count();
        for n in snapshot.names() {
            if n == name {
                continue;
            }
            if !in_scope(target_parent, &n, scope) {
                continue;
            }
            let other = match snapshot.get(&n) {
                Some(o) => o,
                None => continue,
            };
            let (common, _ta, _tb) =
                holofs_analytics::fingerprint::shard_overlap(&manifest, other);
            if common > 0 {
                let pct = if total_a > 0 {
                    common as f32 * 100.0 / total_a as f32
                } else {
                    0.0
                };
                // Stage 13.0: compute low / high band overlaps so the
                // UI can surface a "robust copy?" warning. Split point
                // is the midpoint of `nlayers` — for `nlayers=8` that's
                // L0..=L3 vs L4..=L7. For files with `nlayers < 2`
                // (text / opaque) both bands collapse to zero, which
                // the UI treats as "n/a".
                let per_layer =
                    holofs_analytics::fingerprint::shard_overlap_per_layer(&manifest, other);
                let nlayers = per_layer.len();
                let mid = nlayers / 2;
                let low_shared: u32 = per_layer.iter().take(mid).sum();
                let high_shared: u32 = per_layer.iter().skip(mid).sum();
                let low_total: usize = manifest
                    .shard_hashes
                    .iter()
                    .flat_map(|chan| chan.iter().take(mid))
                    .map(|hs| hs.len())
                    .sum();
                let high_total: usize = manifest
                    .shard_hashes
                    .iter()
                    .flat_map(|chan| chan.iter().skip(mid))
                    .map(|hs| hs.len())
                    .sum();
                let low_pct = if low_total > 0 {
                    low_shared as f32 * 100.0 / low_total as f32
                } else {
                    0.0
                };
                let high_pct = if high_total > 0 {
                    high_shared as f32 * 100.0 / high_total as f32
                } else {
                    0.0
                };
                overlaps.push(ShardOverlap {
                    name: n,
                    common,
                    overlap_pct: pct,
                    low_layer_overlap_pct: low_pct,
                    high_layer_overlap_pct: high_pct,
                    robust_copy_score: low_pct - high_pct,
                });
            }
        }
        overlaps.sort_by_key(|o| std::cmp::Reverse(o.common));

        let fingerprint_hex = if is_text {
            holofs_analytics::shingle::minhash_hex_preview(&manifest.text_minhash)
        } else {
            holofs_analytics::fingerprint::fingerprint_hex(&target_fp)
        };

        Ok(SimilarReport {
            name: name.to_string(),
            kind: manifest.kind,
            fingerprint_hex,
            minhash_k: holofs_analytics::shingle::MINHASH_K,
            total_shards: total_a,
            neighbors,
            overlaps,
        })
    }

    // `diff_chunks` + Diff{Cell,Layer,Report} moved to `diff.rs` in R1b.10.
    // `mix_objects` + `filter_audio` moved to `mix.rs` in Phase R1b.9.

    /// Stage 12.7: per-file metrics for `/health/<name>`.
    ///
    /// Three classes of signal collapsed into one server roundtrip:
    ///
    /// 1. **Storage / deduplication** — count of unique shard hashes
    ///    inside this file, inside the catalog as a whole, and how many
    ///    of *this* file's hashes are not seen anywhere else. Gives a
    ///    direct «how much exclusive content this file contributes»
    ///    number that is impossible to get from raw byte counts.
    ///
    /// 2. **Originality + structural neighbours** — for every other
    ///    file we tally `(shared_total, shared_per_layer)` against the
    ///    target. Two files that share the coarsest layers but diverge
    ///    higher up are "same composition, different detail"; the
    ///    inverse pattern says "same texture / detail, different
    ///    composition". The frontend renders the per-layer split as a
    ///    little bar per neighbour so the kind of similarity is
    ///    legible at a glance.
    ///
    /// 3. **Layer energy** — for image / audio only. We actually decode
    ///    every layer once and sum squared coefficient magnitudes; the
    ///    distribution lets the UI report a "detail score" (energy
    ///    above the median layer / total) and, for audio, a
    ///    bass / mid / treble breakdown via even thirds of the layer
    ///    set.
    pub async fn file_metrics(&self, name: &str) -> Result<FileMetrics, GatewayError> {
        let snapshot = self.catalog.lock().await.clone();
        let manifest = snapshot
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind == ObjectKind::Directory {
            return Err(GatewayError::BadRequest(
                "file_metrics: not applicable to directories".into(),
            ));
        }
        let nlayers = manifest.nlayers as usize;

        // --- 1. Flatten *this file*'s shard hashes per layer ---------------
        // `layer_hashes[l] = set of hashes across all channels of layer l`.
        // We dedupe within the file too so a duplicated coefficient (rare,
        // but possible when sym_len is short) doesn't get double-counted.
        let mut layer_hashes: Vec<std::collections::HashSet<[u8; 32]>> =
            vec![std::collections::HashSet::new(); nlayers];
        let mut total_in_file: u64 = 0;
        for c in 0..manifest.channels as usize {
            for l in 0..nlayers {
                if let Some(hs) = manifest
                    .shard_hashes
                    .get(c)
                    .and_then(|chan| chan.get(l))
                {
                    total_in_file += hs.len() as u64;
                    for h in hs {
                        layer_hashes[l].insert(*h);
                    }
                }
            }
        }
        let unique_in_file: u64 = layer_hashes.iter().map(|s| s.len() as u64).sum();
        let all_in_file: std::collections::HashSet<[u8; 32]> =
            layer_hashes.iter().flatten().copied().collect();

        // --- 2. Walk other catalog manifests once -------------------------
        //
        // For each other file we accumulate, per layer of *this* file,
        // how many of its hashes also show up in that other file. The
        // sum across layers is `shared_total`. We also track which of
        // this file's hashes appear in any other file at all, which
        // gives us the per-layer + overall originality counts.
        let mut foreign_seen: std::collections::HashSet<[u8; 32]> =
            std::collections::HashSet::new();
        let mut catalog_total_shards: u64 = 0;
        let mut catalog_unique_set: std::collections::HashSet<[u8; 32]> =
            std::collections::HashSet::new();
        let mut neighbours: Vec<NeighbourMetric> = Vec::new();

        for n in snapshot.names() {
            let other = match snapshot.get(&n) {
                Some(m) => m,
                None => continue,
            };
            if other.kind == ObjectKind::Directory {
                continue;
            }
            // Catalog-wide accounting (always — includes self).
            for c in 0..other.channels as usize {
                let nlayers_o = other.nlayers as usize;
                for l in 0..nlayers_o {
                    if let Some(hs) = other
                        .shard_hashes
                        .get(c)
                        .and_then(|chan| chan.get(l))
                    {
                        catalog_total_shards += hs.len() as u64;
                        for h in hs {
                            catalog_unique_set.insert(*h);
                        }
                    }
                }
            }
            if n == name {
                continue;
            }
            // Neighbour: per-(this-file) layer shared count.
            let mut per_layer = vec![0u32; nlayers];
            let mut shared_total: u64 = 0;
            for c in 0..other.channels as usize {
                let nlayers_o = other.nlayers as usize;
                for l in 0..nlayers_o {
                    if let Some(hs) = other
                        .shard_hashes
                        .get(c)
                        .and_then(|chan| chan.get(l))
                    {
                        for h in hs {
                            if all_in_file.contains(h) {
                                foreign_seen.insert(*h);
                                shared_total += 1;
                                // Bucket the shared hash under the layer of
                                // *this* file it lives in.
                                for (li, set) in layer_hashes.iter().enumerate().take(nlayers) {
                                    if set.contains(h) {
                                        per_layer[li] += 1;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if shared_total > 0 {
                let pct = if unique_in_file > 0 {
                    shared_total as f32 * 100.0 / unique_in_file as f32
                } else {
                    0.0
                };
                neighbours.push(NeighbourMetric {
                    name: n,
                    kind: other.kind,
                    shared_total,
                    shared_per_layer: per_layer,
                    overlap_pct: pct,
                });
            }
        }
        neighbours.sort_by_key(|m| std::cmp::Reverse(m.shared_total));
        neighbours.truncate(8);

        let unique_to_file: u64 = all_in_file
            .iter()
            .filter(|h| !foreign_seen.contains(*h))
            .count() as u64;
        let originality_pct = if !all_in_file.is_empty() {
            unique_to_file as f32 * 100.0 / all_in_file.len() as f32
        } else {
            0.0
        };
        // Per-layer originality: of this layer's hashes, how many are NOT in any
        // other file?
        let mut originality_per_layer: Vec<f32> = Vec::with_capacity(nlayers);
        for set in &layer_hashes {
            if set.is_empty() {
                originality_per_layer.push(0.0);
            } else {
                let exclusive = set.iter().filter(|h| !foreign_seen.contains(*h)).count();
                originality_per_layer
                    .push(exclusive as f32 * 100.0 / set.len() as f32);
            }
        }

        // --- 3. Layer energy (image / audio only) -------------------------
        //
        // The decode does a network roundtrip per (channel, layer) — same
        // weight as a Monte Carlo health pass — so we gate it on object
        // kind. Anything that can't be Haar-decoded (Opaque, Text)
        // returns `None` and the UI hides the block.
        let layer_energy = if matches!(manifest.kind, ObjectKind::Image | ObjectKind::Audio) {
            let live = self.effective_live().await;
            match layer_energies(&self.gf, &manifest, &live).await {
                Ok((energies, _bytes)) => Some(energies),
                Err(_) => None,
            }
        } else {
            None
        };

        // For audio, group layers into three equal bands. Layer 0 = coarsest
        // = bass envelope; the highest layer = treble. We sum energies
        // across consecutive layer bins.
        let audio_bands = if manifest.kind == ObjectKind::Audio {
            if let Some(energies) = layer_energy.as_ref() {
                let n = energies.len();
                if n == 0 {
                    None
                } else {
                    let third = (n + 2) / 3;
                    let bass = energies.iter().take(third).sum::<f64>();
                    let mid = energies
                        .iter()
                        .skip(third)
                        .take(third)
                        .sum::<f64>();
                    let treble = energies.iter().skip(2 * third).sum::<f64>();
                    Some(AudioBandEnergy { bass, mid, treble })
                }
            } else {
                None
            }
        } else {
            None
        };

        Ok(FileMetrics {
            name: name.to_string(),
            kind: manifest.kind,
            // Storage / dedup.
            total_shards_in_file: total_in_file,
            unique_shards_in_file: unique_in_file,
            file_dedup_savings_pct: if total_in_file > 0 {
                (total_in_file - unique_in_file) as f32 * 100.0 / total_in_file as f32
            } else {
                0.0
            },
            catalog_total_shards,
            catalog_unique_shards: catalog_unique_set.len() as u64,
            unique_to_file,
            originality_pct,
            originality_per_layer,
            // Cross-file reuse.
            neighbours,
            // Decoded.
            layer_energy,
            audio_bands,
        })
    }
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

/// Result of [`Gateway::fingerprint_of`].
#[derive(Debug, Clone)]
pub struct FingerprintInfo {
    /// Catalog name.
    pub name: String,
    /// 32-char lowercase hex of the 16-byte fingerprint.
    pub fingerprint_hex: String,
    /// Object kind — controls how the fingerprint was computed.
    pub kind: holofs_model::manifest::ObjectKind,
}

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

/// One row of [`FileMetrics::neighbours`] — a catalog entry that shares
/// at least one shard hash with the target file. Holofs's content-addressed
/// shard storage means we can describe "how much of file A also lives in
/// file B" with no decoding and no perceptual hashing.
#[derive(Debug, Clone)]
pub struct NeighbourMetric {
    /// Catalog name of the other file.
    pub name: String,
    /// Object kind, so the UI can render different glyphs for image/audio/etc.
    pub kind: ObjectKind,
    /// Total shard hashes shared (summed across channels and layers).
    pub shared_total: u64,
    /// Per-layer split of `shared_total`, indexed by *this* file's layer
    /// (`shared_per_layer.len() == target.nlayers`). Lets the UI tell apart
    /// "shares the coarse structure" from "shares the fine details".
    pub shared_per_layer: Vec<u32>,
    /// `shared_total` as a percentage of the target's unique hashes.
    pub overlap_pct: f32,
}

/// Result of [`Gateway::file_metrics`] — bundle of business-meaningful
/// signals about one catalog entry that fall out of the holofs storage
/// geometry (per-layer shards + content-addressed hashes) for free.
#[derive(Debug, Clone)]
pub struct FileMetrics {
    pub name: String,
    pub kind: ObjectKind,

    // --- Storage / dedup -------------------------------------------------
    /// Total shard hashes the file would occupy if every hash were unique.
    pub total_shards_in_file: u64,
    /// Distinct shard hashes inside the file (≤ total; equal in the common
    /// case — drops below total only when an RLNC encoding happens to
    /// repeat a hash, which is rare).
    pub unique_shards_in_file: u64,
    /// `(total - unique) / total * 100`.
    pub file_dedup_savings_pct: f32,
    /// Catalog-wide total shards.
    pub catalog_total_shards: u64,
    /// Distinct shard hashes across the entire catalog (cluster-wide
    /// dedup denominator).
    pub catalog_unique_shards: u64,

    // --- Originality -----------------------------------------------------
    /// Number of this file's distinct hashes that do not appear in any
    /// other catalog entry — the file's exclusive contribution.
    pub unique_to_file: u64,
    /// `unique_to_file / distinct_hashes_in_file * 100`.
    pub originality_pct: f32,
    /// Per-layer originality percentage (length = `nlayers`). High values
    /// in low layers ⇒ unique structure; high values in high layers ⇒
    /// unique detail. The combination is the storytelling axis.
    pub originality_per_layer: Vec<f32>,

    // --- Cross-file reuse ------------------------------------------------
    /// Top-N (currently 8) other catalog entries with the most shared
    /// hashes, sorted by `shared_total` descending. Empty if the file
    /// has no neighbours.
    pub neighbours: Vec<NeighbourMetric>,

    // --- Decoded layer energy (image / audio only) ----------------------
    /// Sum of squared DWT coefficients per layer (channels summed).
    /// `None` when not applicable (Text, Opaque) or when the decode
    /// failed (e.g. too few live shards).
    pub layer_energy: Option<Vec<f64>>,
    /// Audio-only: three-band energy split derived from `layer_energy`.
    pub audio_bands: Option<AudioBandEnergy>,
}


/// Three-band audio energy split returned in [`FileMetrics::audio_bands`].
/// Bands are an equal-thirds partition of the layer set, mapped to
/// rough frequency labels: layer 0 = bass envelope, the top layers =
/// treble transients.
#[derive(Debug, Clone, Copy)]
pub struct AudioBandEnergy {
    /// Energy in the bottom third of the layer set (bass / envelope).
    pub bass: f64,
    /// Energy in the middle third of the layer set.
    pub mid: f64,
    /// Energy in the top third of the layer set (treble / transients).
    pub treble: f64,
}

// === Stage 8 holographic key escrow ==================================
//
// EscrowShareInfo + EscrowSplitResult + EscrowShareBytes +
// EscrowRecoverResult and the three escrow impl Gateway methods
// moved to `escrow.rs` in Phase R1b.5.


/// Guess the content-type of an arbitrary binary by extension. If unknown,
/// fall back to `application/octet-stream` (universal "untyped binary").
// guess_opaque_content_type, guess_text_content_type, encode_png —
// moved to `util.rs` in Phase R1b.1. The `use` at the top of this
// file brings them back into scope with the same names.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_object_id_is_deterministic_and_path_sensitive() {
        let a1 = directory_object_id("photos");
        let a2 = directory_object_id("photos");
        let b = directory_object_id("photos/2026");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_ne!(a1, 0);
    }

    #[test]
    fn parent_dir_strips_last_segment() {
        assert_eq!(parent_dir("a/b/c.png"), "a/b");
        assert_eq!(parent_dir("top.png"), "");
        assert_eq!(parent_dir("only/one.png"), "only");
    }

    #[test]
    fn scope_all_keeps_everything() {
        assert!(in_scope("photos/2024", "anything/else.png", SimilarScope::All));
        assert!(in_scope("", "top.png", SimilarScope::All));
    }

    #[test]
    fn scope_folder_keeps_direct_siblings_only() {
        let p = "photos/2024";
        assert!(in_scope(p, "photos/2024/x.png", SimilarScope::Folder));
        assert!(in_scope(p, "photos/2024/y.jpg", SimilarScope::Folder));
        assert!(!in_scope(p, "photos/2024/sub/z.png", SimilarScope::Folder));
        assert!(!in_scope(p, "photos/2023/x.png", SimilarScope::Folder));
        assert!(!in_scope(p, "top.png", SimilarScope::Folder));
    }

    #[test]
    fn scope_folder_at_root_keeps_only_root_level() {
        assert!(in_scope("", "top.png", SimilarScope::Folder));
        assert!(!in_scope("", "sub/x.png", SimilarScope::Folder));
    }

    #[test]
    fn scope_tree_keeps_subtree() {
        let p = "photos/2024";
        assert!(in_scope(p, "photos/2024/x.png", SimilarScope::Tree));
        assert!(in_scope(p, "photos/2024/sub/z.png", SimilarScope::Tree));
        assert!(in_scope(p, "photos/2024/sub/deeper/w.png", SimilarScope::Tree));
        // Sibling directory must NOT match — `photos/2024sub` could
        // collide with a naive prefix check, so the helper uses the
        // `parent/` form.
        assert!(!in_scope(p, "photos/2024sub/x.png", SimilarScope::Tree));
        assert!(!in_scope(p, "photos/2023/x.png", SimilarScope::Tree));
        assert!(!in_scope(p, "top.png", SimilarScope::Tree));
    }

    #[test]
    fn scope_tree_at_root_spans_everything() {
        assert!(in_scope("", "top.png", SimilarScope::Tree));
        assert!(in_scope("", "sub/deep/x.png", SimilarScope::Tree));
    }

    #[test]
    fn scope_parse_unknown_falls_back_to_all() {
        assert_eq!(SimilarScope::parse("folder"), SimilarScope::Folder);
        assert_eq!(SimilarScope::parse("tree"), SimilarScope::Tree);
        assert_eq!(SimilarScope::parse("all"), SimilarScope::All);
        assert_eq!(SimilarScope::parse(""), SimilarScope::All);
        assert_eq!(SimilarScope::parse("garbage"), SimilarScope::All);
    }
}
