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

use tokio::sync::Mutex;

use holofs_client::{
    get_audio_filtered, get_object_up_to_layer, get_object_with_coeff_mask, layer_energies,
    mix_images_at_split, purge_object, put_object, LiveNodes,
};
use holofs_codec::image_io::{load_photo_from_bytes, to_rgb};
use holofs_core::gf::Gf;
use holofs_core::hash::{hex, sha256};
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
    catalog: Arc<Mutex<Directory>>,
    /// Optional path to the catalog file. If set, the catalog is saved
    /// atomically on each change (PUT/DELETE).
    catalog_path: Option<std::path::PathBuf>,
    /// Stage 12.8: optional semantic-search embeddings index. `None`
    /// when the server was started without `--enable-embed`. When set,
    /// every PUT fires a fire-and-forget background task that embeds
    /// the new object via CLIP and appends to the on-disk index.
    embed: Arc<Mutex<EmbedState>>,
    /// Stage 13.4: optional per-object version history. When enabled
    /// every PUT that *replaces* an existing object writes the prior
    /// manifest as a side file under `versions_dir/<sanitized>/v…bin`
    /// and skips the usual shard purge so the historical version
    /// remains decodeable. Trade-off: cluster storage monotonically
    /// grows while the feature is on (no GC yet).
    versions: Arc<Mutex<VersionsState>>,
    gf: Arc<Gf>,
    /// Baseline list of "actually live" cluster nodes. `admin_kills` flags
    /// (set via the UI) are layered on top of it.
    live: Arc<LiveNodes>,
    /// "Node disabled by admin" flags indexed by `cluster.node_addrs`.
    /// The node keeps responding physically, but the gateway treats it as dead:
    /// PUT/GET bypass it, the health-monitor sees the margin drop, the auditor
    /// does not query it.
    admin_kills: Arc<Mutex<Vec<bool>>>,
    cluster: Arc<ClusterInfo>,
    /// Cache keyed by (name, max_decoded_layer) → ready PNG + metrics.
    cache: Mutex<HashMap<(String, u8), Arc<CachedFile>>>,
    /// Stage 11.2: per-`(name, channel, layer)` shard cache. `shard_payload`
    /// previously called `gather_layer` for every cell on `/inspect/<name>`
    /// — under the inspect grid's ~444 concurrent renders that saturated
    /// the cluster and produced spurious 404s for cells whose layer fetch
    /// raced. The `OnceCell` deduplicates concurrent gathers: the first
    /// caller does the work, every other caller awaits the same future.
    /// Invalidated together with [`Self::cache`] on every PUT / DELETE.
    shard_cache: Mutex<
        HashMap<(String, u8, u8), Arc<tokio::sync::OnceCell<Arc<Vec<holofs_core::rlnc::Shard>>>>>,
    >,
    /// Temporary cache of generated escrow shares: escrow_id_hex → Vec<ShareFile>.
    /// Kept only until the gateway restarts (shares are not part of the cluster).
    escrow_cache: Mutex<HashMap<String, Vec<holofs_analytics::escrow::ShareFile>>>,
}

struct CachedFile {
    bytes: Vec<u8>,
    max_layer: u8,
    bytes_downloaded: u64,
    decode_ms: u128,
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
            gf,
            live,
            admin_kills: Arc::new(Mutex::new(vec![false; n])),
            cluster,
            cache: Mutex::new(HashMap::new()),
            shard_cache: Mutex::new(HashMap::new()),
            escrow_cache: Mutex::new(HashMap::new()),
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
            gf,
            live,
            admin_kills: Arc::new(Mutex::new(vec![false; n])),
            cluster,
            cache: Mutex::new(HashMap::new()),
            shard_cache: Mutex::new(HashMap::new()),
            escrow_cache: Mutex::new(HashMap::new()),
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
        }
    }

    async fn get_or_decode(&self, name: &str, max_layer: u8) -> Option<Arc<CachedFile>> {
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&(name.to_string(), max_layer)) {
                return Some(Arc::clone(c));
            }
        }
        let manifest = self.catalog.lock().await.get(name)?.clone();
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes) = get_object_up_to_layer(&self.gf, &manifest, &live, max_layer)
            .await
            .ok()?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, manifest.width, manifest.height);
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

/// Summary of a successful DELETE.
#[derive(Debug, Clone)]
pub struct RemoveResult {
    /// Catalog name that was removed.
    pub name: String,
    /// 64-bit `object_id` whose shards were Purged.
    pub object_id: u64,
}

/// Summary of a successful `mkdir`.
#[derive(Debug, Clone)]
pub struct MkdirResult {
    /// Catalog path of the new directory entry.
    pub path: String,
    /// Stable directory object id (SHA-256-derived from the path).
    pub object_id: u64,
}

/// Summary of a successful `rmdir`.
#[derive(Debug, Clone)]
pub struct RmdirResult {
    /// Catalog path that was removed.
    pub path: String,
    /// Object id of the removed directory marker.
    pub object_id: u64,
}

/// Summary of a successful `rename`. For directories `moved_entries` is
/// `1 + descendant_count`; for files it is always `1`.
#[derive(Debug, Clone)]
pub struct RenameResult {
    /// Source catalog path.
    pub old: String,
    /// Destination catalog path.
    pub new: String,
    /// Number of catalog entries that were rewritten (the entry itself
    /// plus every descendant when renaming a directory).
    pub moved_entries: usize,
}

/// Current Unix epoch seconds. Stamped onto every PUT'd manifest and
/// every newly created `Directory` marker so the catalog can be sorted
/// by creation time later. Falls back to `0` if the clock is somehow
/// behind the epoch (we don't want to panic the whole ingest path on
/// what should be impossible).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable, path-derived id for directory markers. We hash with a separate
/// domain tag so a collision with a data object id is impossible.
fn directory_object_id(path: &str) -> u64 {
    let mut buf = Vec::with_capacity(path.len() + 16);
    buf.extend_from_slice(b"holofs-dir-v1\0");
    buf.extend_from_slice(path.as_bytes());
    let h = sha256(&buf);
    let mut id = [0u8; 8];
    id.copy_from_slice(&h[..8]);
    u64::from_be_bytes(id)
}

/// Errors the public Gateway API can return. Frontends map them to HTTP
/// status codes (404, 400, 503, etc.).
#[derive(Debug, Clone)]
pub enum GatewayError {
    /// Object not in catalog.
    NotFound,
    /// Client-side error (empty body, bad name, unsupported kind).
    BadRequest(String),
    /// Decode failed at the cluster level (not enough shards, network).
    Decode(String),
    /// Preview was requested for text/opaque — no graceful projection exists.
    PreviewUnsupported,
    /// Caller asked for the bytes of a `Directory` entry. Directories have
    /// no payload; the HTTP layer surfaces this as `409 Conflict`.
    IsDirectory,
    /// An entry already exists at the target path. `mkdir` returns this for
    /// any non-directory entry; PUT returns it for directory entries.
    AlreadyExists,
    /// `rmdir`/`list_dir` invoked on a path that exists but is not a
    /// `Directory` entry.
    NotADirectory,
    /// `rmdir` invoked on a directory that still has children. The frontend
    /// surfaces this as `409 Conflict`.
    DirectoryNotEmpty,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::NotFound => write!(f, "not found"),
            GatewayError::BadRequest(s) => write!(f, "bad request: {s}"),
            GatewayError::Decode(s) => write!(f, "decode: {s}"),
            GatewayError::PreviewUnsupported => write!(f, "preview not supported for this kind"),
            GatewayError::IsDirectory => write!(f, "is a directory"),
            GatewayError::AlreadyExists => write!(f, "already exists"),
            GatewayError::NotADirectory => write!(f, "not a directory"),
            GatewayError::DirectoryNotEmpty => write!(f, "directory not empty"),
        }
    }
}

impl std::error::Error for GatewayError {}

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
            } else if let Err(e) = purge_object(old, &live).await {
                eprintln!("PUT {name}: previous object failed to purge (continuing): {e}");
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

    /// Delete an object: remove from catalog, persist, Purge shards on
    /// every live node. Returns `NotFound` if the name is not in the
    /// catalog, `Decode` if the cluster Purge partially fails. Directory
    /// entries cannot be deleted via this method — use [`rmdir`].
    pub async fn remove_object(&self, name: &str) -> Result<RemoveResult, GatewayError> {
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.lock().await;
        if matches!(cat.get(name), Some(m) if m.kind == ObjectKind::Directory) {
            return Err(GatewayError::IsDirectory);
        }
        let manifest = cat.remove(name).ok_or(GatewayError::NotFound)?;
        drop(cat);
        self.invalidate_cache(name).await;
        self.persist_catalog().await;
        let live = self.effective_live().await;
        purge_object(&manifest, &live)
            .await
            .map_err(|e| GatewayError::Decode(format!("partial purge: {e}")))?;
        Ok(RemoveResult {
            name: name.to_string(),
            object_id: manifest.object_id,
        })
    }

    /// Create a `Directory` entry at `path`. The parent (if any) must
    /// already exist as a directory; the path itself must not be taken.
    /// Returns `AlreadyExists` (target taken), `NotADirectory` (parent is
    /// not a directory), `BadRequest` (parent missing or path malformed).
    pub async fn mkdir(&self, path: &str) -> Result<MkdirResult, GatewayError> {
        catalog_path::validate(path)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.lock().await;
        if cat.get(path).is_some() {
            return Err(GatewayError::AlreadyExists);
        }
        if let Some(parent) = catalog_path::parent(path) {
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
        let manifest = Manifest::directory(directory_object_id(path), now_unix());
        let object_id = manifest.object_id;
        cat.insert(path.to_string(), manifest);
        drop(cat);
        self.persist_catalog().await;
        Ok(MkdirResult {
            path: path.to_string(),
            object_id,
        })
    }

    /// Remove an empty directory. Errors:
    /// - `NotFound`: no entry at `path`.
    /// - `NotADirectory`: entry exists but is a data object.
    /// - `DirectoryNotEmpty`: at least one entry has `path` as a prefix.
    pub async fn rmdir(&self, path: &str) -> Result<RmdirResult, GatewayError> {
        catalog_path::validate(path)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.lock().await;
        match cat.get(path) {
            Some(m) if m.kind == ObjectKind::Directory => {}
            Some(_) => return Err(GatewayError::NotADirectory),
            None => return Err(GatewayError::NotFound),
        }
        // BTreeMap range scan: anything that starts with `path + "/"` is a
        // descendant. The first such key is enough to refuse the call.
        let child_prefix = format!("{path}/");
        if cat
            .entries
            .range(child_prefix.clone()..)
            .next()
            .map(|(k, _)| k.starts_with(&child_prefix))
            .unwrap_or(false)
        {
            return Err(GatewayError::DirectoryNotEmpty);
        }
        let removed = cat.remove(path).expect("checked above");
        drop(cat);
        self.persist_catalog().await;
        Ok(RmdirResult {
            path: path.to_string(),
            object_id: removed.object_id,
        })
    }

    /// Rename an entry. For a directory all descendants are rewritten too;
    /// the rename is atomic w.r.t. the catalog mutex but **not** w.r.t. the
    /// on-disk catalog (a crash between mutation and persist could leave
    /// the old name visible after restart — same as PUT/DELETE today).
    ///
    /// Errors:
    /// - `NotFound`: no entry at `old`.
    /// - `AlreadyExists`: an entry already lives at `new` (or any
    ///   descendant target collides during a directory rename).
    /// - `BadRequest`: parent of `new` missing, or `new` would be a
    ///   descendant of `old` (cycle).
    pub async fn rename(&self, old: &str, new: &str) -> Result<RenameResult, GatewayError> {
        catalog_path::validate(old)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        catalog_path::validate(new)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        if old == new {
            return Ok(RenameResult {
                old: old.to_string(),
                new: new.to_string(),
                moved_entries: 0,
            });
        }
        // Refuse to move a directory into itself.
        if new == old || new.starts_with(&format!("{old}/")) {
            return Err(GatewayError::BadRequest(
                "cannot rename a directory into its own descendant".into(),
            ));
        }
        let mut cat = self.catalog.lock().await;
        let entry = cat.get(old).cloned().ok_or(GatewayError::NotFound)?;
        if cat.get(new).is_some() {
            return Err(GatewayError::AlreadyExists);
        }
        if let Some(parent) = catalog_path::parent(new) {
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
        let mut moved: Vec<(String, String, Manifest)> = Vec::new();
        moved.push((old.to_string(), new.to_string(), entry.clone()));
        if entry.kind == ObjectKind::Directory {
            let child_prefix = format!("{old}/");
            for (k, v) in cat.entries.range(child_prefix.clone()..) {
                if !k.starts_with(&child_prefix) {
                    break;
                }
                let suffix = &k[child_prefix.len()..];
                let target = format!("{new}/{suffix}");
                if cat.get(&target).is_some() {
                    return Err(GatewayError::AlreadyExists);
                }
                moved.push((k.clone(), target, v.clone()));
            }
        }
        let count = moved.len();
        for (from, to, manifest) in moved {
            cat.remove(&from);
            cat.insert(to, manifest);
        }
        drop(cat);
        self.invalidate_cache(old).await;
        self.persist_catalog().await;
        Ok(RenameResult {
            old: old.to_string(),
            new: new.to_string(),
            moved_entries: count,
        })
    }

    /// Immediate children of `prefix`. Pass `""` for the root listing.
    /// Returned tuples are `(full_path, manifest)`; the basename is
    /// `full_path[prefix.len() + 1..]` for non-root prefixes.
    ///
    /// Errors `NotADirectory` if `prefix` is a real entry but not a
    /// directory; `NotFound` if `prefix` is non-empty and unknown.
    pub async fn list_dir(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, Manifest)>, GatewayError> {
        if !prefix.is_empty() {
            catalog_path::validate(prefix)
                .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        }
        let cat = self.catalog.lock().await;
        if !prefix.is_empty() {
            match cat.get(prefix) {
                Some(m) if m.kind == ObjectKind::Directory => {}
                Some(_) => return Err(GatewayError::NotADirectory),
                None => return Err(GatewayError::NotFound),
            }
        }
        let (start, child_prefix_len) = if prefix.is_empty() {
            (String::new(), 0usize)
        } else {
            let cp = format!("{prefix}/");
            let len = cp.len();
            (cp, len)
        };
        let mut out: Vec<(String, Manifest)> = Vec::new();
        for (k, v) in cat.entries.range(start.clone()..) {
            if !prefix.is_empty() && !k.starts_with(&start) {
                break;
            }
            if k == prefix {
                continue;
            }
            // Skip non-immediate descendants: the remainder after the
            // prefix must contain no '/'.
            let remainder = &k[child_prefix_len..];
            if remainder.is_empty() || remainder.contains('/') {
                continue;
            }
            out.push((k.clone(), v.clone()));
        }
        Ok(out)
    }

    /// Snapshot of cluster-wide statistics for `GET /api/stats`.
    pub async fn api_stats(&self) -> ApiStats {
        use std::collections::HashSet;

        let snapshot = self.catalog.lock().await.clone();
        let mut counts = KindCounts::default();
        let mut total_shards = 0u64;
        let mut total_payload_bytes = 0u64;
        let mut unique_hashes: HashSet<holofs_core::merkle::Hash> = HashSet::new();
        for n in snapshot.names() {
            let m = snapshot.get(&n).unwrap();
            match m.kind {
                holofs_model::manifest::ObjectKind::Image => counts.image += 1,
                holofs_model::manifest::ObjectKind::Audio => counts.audio += 1,
                holofs_model::manifest::ObjectKind::Text => counts.text += 1,
                holofs_model::manifest::ObjectKind::Opaque => counts.opaque += 1,
                holofs_model::manifest::ObjectKind::Directory => counts.directory += 1,
            }
            for (l, npl) in m.n_per_layer.iter().enumerate() {
                let bytes_per = m.sym_len.get(l).copied().unwrap_or(0) as u64 + m.k as u64;
                total_shards += u64::from(*npl) * u64::from(m.channels);
                total_payload_bytes += u64::from(*npl) * u64::from(m.channels) * bytes_per;
            }
            for per_c in &m.shard_hashes {
                for per_l in per_c {
                    for h in per_l {
                        unique_hashes.insert(*h);
                    }
                }
            }
        }
        let kills = self.admin_kills.lock().await;
        let nodes_live = kills.iter().filter(|&&k| !k).count();
        let nodes_total = kills.len();
        drop(kills);
        let dedup_pct = if total_shards > 0 {
            (1.0 - unique_hashes.len() as f64 / total_shards as f64) * 100.0
        } else {
            0.0
        };
        ApiStats {
            nodes_total,
            nodes_live,
            objects_total: snapshot.len(),
            objects_by_kind: counts,
            shards_total: total_shards,
            shards_unique: unique_hashes.len() as u64,
            dedup_savings_pct: (dedup_pct * 100.0).round() / 100.0,
            bytes_total: total_payload_bytes,
        }
    }

    /// Snapshot of cluster-wide data needed by `GET /health`. Cheap: only
    /// the catalog mutex + admin_kills snapshot.
    pub async fn health_index_data(&self) -> HealthIndexData {
        let cluster = &self.cluster;
        let kills = self.admin_kills.lock().await.clone();
        let nodes: Vec<NodeStatus> = (0..cluster.node_addrs.len())
            .map(|i| NodeStatus {
                idx: i,
                addr: cluster.node_addrs[i].clone(),
                zone: cluster.zones.get(i).copied().unwrap_or(0),
                admin_killed: kills.get(i).copied().unwrap_or(false),
            })
            .collect();
        let mut objects = self.catalog.lock().await.names();
        objects.sort();
        let n_live = kills.iter().filter(|&&k| !k).count();
        let n_total = kills.len();
        HealthIndexData {
            nodes,
            objects,
            n_live,
            n_total,
        }
    }

    /// Full health report for one object: per-layer margin + Monte Carlo
    /// loss simulation + zone failure scenarios. Wraps
    /// `holofs_cluster::health::object_health`. Polls the cluster (network
    /// I/O) and runs the 5000-trial simulation.
    pub async fn object_health(
        &self,
        name: &str,
    ) -> Result<holofs_cluster::health::ObjectHealth, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let live = self.effective_live().await;
        holofs_cluster::health::object_health(name, &manifest, &live)
            .await
            .map_err(|e| GatewayError::Decode(format!("object_health: {e}")))
    }

    /// Flip the admin-kill flag for `idx`. Clears the PNG cache because the
    /// next decode might pick a different node set. Returns the new state.
    pub async fn toggle_admin_kill(
        &self,
        idx: usize,
    ) -> Result<AdminToggleResult, GatewayError> {
        let mut kills = self.admin_kills.lock().await;
        if idx >= kills.len() {
            return Err(GatewayError::BadRequest(format!(
                "bad index {idx} (cluster has {} nodes)",
                kills.len()
            )));
        }
        kills[idx] = !kills[idx];
        let now_killed = kills[idx];
        drop(kills);
        self.cache.lock().await.clear();
        Ok(AdminToggleResult { idx, now_killed })
    }

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

    /// `/inspect/<name>` view-model: the per-(channel, layer) layout of every
    /// shard for one object. Used by `holofs-web` to render the shard grid.
    pub async fn inspect(&self, name: &str) -> Result<InspectInfo, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let live = self.effective_live().await;
        let k = u32::from(manifest.k);
        let mut layers = Vec::with_capacity(usize::from(manifest.channels) * usize::from(manifest.nlayers));
        for c in 0..manifest.channels {
            for l in 0..manifest.nlayers {
                let n = manifest.n_per_layer[l as usize];
                let mut shards = Vec::with_capacity(n as usize);
                for idx in 0..n {
                    let node_idx = manifest.place_shard(c, l, idx, &live);
                    let node_addr = manifest
                        .nodes
                        .get(node_idx)
                        .cloned()
                        .unwrap_or_default();
                    shards.push(ShardInfo {
                        idx,
                        node_idx,
                        node_addr,
                        is_systematic: idx < k,
                    });
                }
                layers.push(LayerLayout {
                    channel: c,
                    layer: l,
                    n_shards: n,
                    k_systematic: manifest.k,
                    shards,
                });
            }
        }
        Ok(InspectInfo {
            name: name.to_string(),
            kind: manifest.kind,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            k: manifest.k,
            layers,
        })
    }

    /// Pull one shard's payload + coeffs over the wire (verified by hash).
    /// `Ok(None)` = shard not currently retrievable (node dead, lost).
    pub async fn shard_payload(
        &self,
        name: &str,
        c: u8,
        l: u8,
        idx: u32,
    ) -> Result<Option<ShardPayload>, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let hash = manifest
            .shard_hashes
            .get(c as usize)
            .and_then(|cl| cl.get(l as usize))
            .and_then(|hs| hs.get(idx as usize))
            .copied()
            .ok_or_else(|| GatewayError::BadRequest("shard out of range".into()))?;
        let live = self.effective_live().await;
        let node_idx = manifest.place_shard(c, l, idx, &live);
        let node_addr = manifest
            .nodes
            .get(node_idx)
            .cloned()
            .unwrap_or_default();
        let sym_len = manifest
            .sym_len
            .get(l as usize)
            .copied()
            .unwrap_or(0);
        let is_systematic = idx < u32::from(manifest.k);

        // Stage 11.2: cache the per-layer gather. The inspect grid renders
        // every (channel, layer)'s shard cells in parallel — without
        // deduplication, each of the ~26 cells in one layer kicks off its
        // own cluster-wide `gather_layer`, which under load drops some
        // responses and yields spurious 404s. With this cache the first
        // caller does the fetch, every concurrent caller awaits the same
        // future, and subsequent calls hit the Arc.
        let key = (name.to_string(), c, l);
        let cell = {
            let mut sc = self.shard_cache.lock().await;
            Arc::clone(
                sc.entry(key)
                    .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
            )
        };
        let manifest_for_init = manifest.clone();
        let live_for_init = live.clone();
        let layer_shards = cell
            .get_or_init(|| async move {
                let v = holofs_client::gather_layer(&manifest_for_init, &live_for_init, c, l)
                    .await
                    .unwrap_or_default();
                Arc::new(v)
            })
            .await
            .clone();

        let shard = layer_shards
            .iter()
            .find(|sh| holofs_core::merkle::shard_hash(sh) == hash)
            .cloned();
        Ok(shard.map(|sh| ShardPayload {
            payload: sh.payload,
            coeffs: sh.coeffs,
            is_systematic,
            hash_hex: hex(&hash),
            node_idx,
            node_addr,
            sym_len,
        }))
    }

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

    /// `/diff/<a>/<b>` view-model: per-(channel, layer) chunk diff cells +
    /// aggregated counters.
    ///
    /// `chunk_diff` is intentionally a **byte-perfect dedup analyzer**.
    /// Two cells are green only when the corresponding systematic shard
    /// hashes are identical — i.e. the underlying source chunks are
    /// byte-for-byte the same. Stage 11.7-11.8 experimented with
    /// perceptual variants (mean-based, then per-coefficient L1) to make
    /// blurred copies "look closer", but every threshold had pathological
    /// neighbours: desaturation that preserves luminance scored higher
    /// than blur; mandala outscored real photos. Perceptual ranking is
    /// what `/similar/<name>` is for; `/diff` stays the dedup tool.
    pub async fn diff_chunks(
        &self,
        name_a: &str,
        name_b: &str,
    ) -> Result<DiffReport, GatewayError> {
        let snapshot = self.catalog.lock().await.clone();
        let a = snapshot
            .get(name_a)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let b = snapshot
            .get(name_b)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if a.kind != b.kind {
            return Err(GatewayError::BadRequest(
                "diff: both objects must be of the same kind".into(),
            ));
        }

        let diff = holofs_analytics::fingerprint::chunk_diff(&a, &b);

        // Group cells by (channel, layer).
        let mut by_layer: std::collections::BTreeMap<(u8, u8), Vec<DiffCell>> =
            std::collections::BTreeMap::new();
        for e in &diff.entries {
            by_layer
                .entry((e.channel, e.layer))
                .or_default()
                .push(DiffCell {
                    idx: e.idx,
                    is_common: e.is_common,
                });
        }
        let layers: Vec<DiffLayer> = by_layer
            .into_iter()
            .map(|((c, l), cells)| {
                let n_common = cells.iter().filter(|x| x.is_common).count();
                let n_total = cells.len();
                DiffLayer {
                    channel: c,
                    layer: l,
                    n_common,
                    n_total,
                    cells,
                }
            })
            .collect();

        let sym_len_l0 = a.sym_len.first().copied().unwrap_or(0) as u64;
        let storage_saved_bytes = diff.common as u64 * sym_len_l0;

        Ok(DiffReport {
            name_a: name_a.to_string(),
            name_b: name_b.to_string(),
            kind: a.kind,
            common: diff.common,
            total: diff.total,
            similarity_pct: diff.similarity_pct(),
            storage_saved_bytes,
            layers,
        })
    }

    /// Stage 12.5: wavelet mix. Build a hybrid PNG where DWT layers
    /// `0..=split` of every channel come from `name_a` and layers
    /// `>split` from `name_b`. Both objects must be images that share
    /// width / height / channels / k / nlayers / per-layer sym_len and
    /// position tables — `mix_images_at_split` returns
    /// `ClientError::Incompatible` otherwise.
    pub async fn mix_objects(
        &self,
        name_a: &str,
        name_b: &str,
        split: u8,
    ) -> Result<MixedImage, GatewayError> {
        let snapshot = self.catalog.lock().await.clone();
        let a = snapshot
            .get(name_a)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let b = snapshot
            .get(name_b)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if a.kind != ObjectKind::Image || b.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "mix: both objects must be images".into(),
            ));
        }
        if split >= a.nlayers {
            return Err(GatewayError::BadRequest(format!(
                "mix: split={split} out of range (image has {} layers, 0..={} valid)",
                a.nlayers,
                a.nlayers.saturating_sub(1)
            )));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) = mix_images_at_split(&self.gf, &a, &b, &live, split)
            .await
            .map_err(|e| match e {
                holofs_client::ClientError::Incompatible(msg) => GatewayError::BadRequest(msg),
                other => GatewayError::Decode(format!("mix: {other}")),
            })?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, a.width, a.height);
        Ok(MixedImage {
            bytes: png,
            width: a.width,
            height: a.height,
            channels: a.channels,
            split_layer: split,
            nlayers: a.nlayers,
            bytes_downloaded: bytes_dl,
            decode_ms,
        })
    }

    /// Stage 12.5: audio layer filter. Decode the object but include
    /// coefficients only from layers whose `keep[layer]` bit is set —
    /// missing layers contribute zero before the inverse Haar. Each
    /// layer roughly maps to a frequency band (L0 = bass envelope, the
    /// highest = treble), so this lets a caller emit lowpass / highpass
    /// / single-band cuts without rebuilding the file.
    pub async fn filter_audio(
        &self,
        name: &str,
        keep: &[bool],
    ) -> Result<FilteredAudio, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Audio {
            return Err(GatewayError::BadRequest(
                "audio filter: object is not audio".into(),
            ));
        }
        if keep.is_empty() {
            return Err(GatewayError::BadRequest(
                "audio filter: keep list is empty (would yield silence)".into(),
            ));
        }
        if !keep.iter().any(|&b| b) {
            return Err(GatewayError::BadRequest(
                "audio filter: every layer is dropped (would yield silence)".into(),
            ));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) = get_audio_filtered(&self.gf, &manifest, &live, keep)
            .await
            .map_err(|e| GatewayError::Decode(format!("audio filter: {e}")))?;
        let decode_ms = t0.elapsed().as_millis();
        let wav = holofs_codec::audio_codec::encode_wav_16bit(&channels, manifest.audio_sample_rate);
        let kept_layers: Vec<u8> = keep
            .iter()
            .enumerate()
            .filter_map(|(i, &k)| k.then_some(i as u8))
            .collect();
        Ok(FilteredAudio {
            bytes: wav,
            sample_rate: manifest.audio_sample_rate,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            kept_layers,
            bytes_downloaded: bytes_dl,
            decode_ms,
        })
    }

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

// === Stage 12.8: CLIP-based semantic search =================================

/// Lazy embedder state shared across the gateway. `enabled` is set via
/// [`Gateway::enable_embed`]; the [`Embedder`](holofs_embed::Embedder)
/// itself is constructed on the first PUT or query after that, so a
/// server that never gets asked to embed pays nothing.
#[derive(Default)]
struct EmbedState {
    enabled: bool,
    index_path: Option<std::path::PathBuf>,
    embedder: Option<Arc<holofs_embed::Embedder>>,
    /// Stage 14.2: in-memory ANN index. `None` until the first
    /// `semantic_search` after a PUT (or after a startup) — then
    /// built from the entire `embeddings.bin`. Bumped to `None` by
    /// `ann_generation` mismatches so the next query rebuilds.
    ann: Option<Arc<holofs_embed::HnswIndex>>,
    /// Increments every time a new embedding is appended. Compared
    /// against the generation the cached `ann` was built at — when
    /// they diverge we drop the cache and rebuild.
    ann_generation: u64,
    /// Generation `ann` was built at. `None` until first build.
    ann_built_at: Option<u64>,
}

/// Layer-band selector for [`Gateway::semantic_search`]. `Any` (the
/// default) takes the best score across all bands per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBand {
    /// L0 reconstruction — silhouette / colour blob.
    Coarse,
    /// L0-L2 reconstruction — silhouette + low-freq detail.
    Mid,
    /// All layers — texture / fine detail.
    Full,
    /// Search all three bands and keep the best score per file.
    Any,
}

impl SearchBand {
    /// Parse from URL string (`coarse` / `mid` / `full` / `any`).
    /// Unknown / empty values map to `Any`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "coarse" | "structure" => Self::Coarse,
            "mid" => Self::Mid,
            "full" | "texture" | "detail" => Self::Full,
            _ => Self::Any,
        }
    }
}

/// One row of [`Gateway::semantic_search`] output. The gateway returns
/// catalog names + scores; the frontend renders them as a card grid.
#[derive(Debug, Clone)]
pub struct SemanticHit {
    /// Catalog name of the matching object.
    pub name: String,
    /// Cosine similarity to the query in `[-1.0, 1.0]`. CLIP-base
    /// scores cluster narrowly around 0.2-0.35 even for strong matches,
    /// so the UI usually shows them as 0..100 percentiles instead of
    /// raw values.
    pub score: f32,
    /// Stage 13.3: which layer band produced the winning score. For
    /// queries filtered to one band this is always that band; for
    /// `SearchBand::Any` it's whichever of the three scored best.
    pub band: SearchBand,
}

impl Gateway {
    /// Lazily build the [`Embedder`](holofs_embed::Embedder) handle.
    /// The first call downloads ~155 MiB of CLIP weights from
    /// HuggingFace into `~/.cache/huggingface/hub`; the next process
    /// boot reads from the cache in milliseconds. Returns `None` when
    /// the embed feature is disabled.
    async fn ensure_embedder(
        &self,
    ) -> Result<Option<Arc<holofs_embed::Embedder>>, GatewayError> {
        // Fast path — already initialised.
        {
            let s = self.embed.lock().await;
            if !s.enabled {
                return Ok(None);
            }
            if let Some(e) = &s.embedder {
                return Ok(Some(Arc::clone(e)));
            }
        }
        // Slow path: spawn_blocking around the candle init.
        let built = tokio::task::spawn_blocking(holofs_embed::Embedder::new)
            .await
            .map_err(|e| GatewayError::BadRequest(format!("embed init join: {e}")))?
            .map_err(|e| GatewayError::BadRequest(format!("embed init: {e}")))?;
        let arc = Arc::new(built);
        let mut s = self.embed.lock().await;
        // Another task may have raced us.
        if let Some(e) = &s.embedder {
            return Ok(Some(Arc::clone(e)));
        }
        s.embedder = Some(Arc::clone(&arc));
        Ok(Some(arc))
    }

    /// Decode the coarse layers of an image-kind object, run CLIP, and
    /// append the embedding to the on-disk index. Idempotent —
    /// `data_cid` re-uploads / duplicates are skipped via
    /// `Index::has`. Returns:
    ///   * `Ok(true)` — newly embedded,
    ///   * `Ok(false)` — already in the index (or embed disabled, or
    ///     wrong kind),
    ///   * `Err(...)` — decode / inference failure.
    pub async fn embed_object(&self, name: &str) -> Result<bool, GatewayError> {
        let Some(embedder) = self.ensure_embedder().await? else {
            return Ok(false);
        };
        let index_path = match self.embed.lock().await.index_path.clone() {
            Some(p) => p,
            None => return Ok(false),
        };

        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        // Stage 12.8 only embeds images. Audio / text get their own
        // embedding pipeline in a future stage.
        if manifest.kind != ObjectKind::Image {
            return Ok(false);
        }

        // Stage 13.3: embed three layer bands per image so the
        // /search page can route queries by abstraction level.
        // Coarse = L0 (silhouette / colour blob), Mid = L0-L2
        // (silhouette + low-freq detail), Full = all layers
        // (full-resolution texture). Each band lives as a distinct
        // record in `embeddings.bin` keyed by (data_cid, band) and is
        // independently dedup-able — re-running embed_object on an
        // already-indexed file is cheap because every band short-
        // circuits at the `Index::has` check.
        let data_cid = manifest.data_cid;
        let last_layer = manifest.nlayers.saturating_sub(1);
        let bands: &[(holofs_embed::LayerBand, u8)] = &[
            (holofs_embed::LayerBand::Coarse, 0),
            (holofs_embed::LayerBand::Mid, 2u8.min(last_layer)),
            (holofs_embed::LayerBand::Full, last_layer),
        ];

        let mut any_new = false;
        let live = self.effective_live().await;
        let width = manifest.width;
        let height = manifest.height;
        let n = (width as usize) * (height as usize);

        for &(band, max_layer) in bands {
            // Per-band dedup check first to skip the decode pass.
            let already = {
                let idx_path = index_path.clone();
                tokio::task::spawn_blocking(move || -> Result<bool, GatewayError> {
                    let idx = holofs_embed::Index::open(&idx_path)
                        .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
                    idx.has(&data_cid, band)
                        .map_err(|e| GatewayError::BadRequest(format!("embed has: {e}")))
                })
                .await
                .map_err(|e| GatewayError::BadRequest(format!("embed has join: {e}")))??
            };
            if already {
                continue;
            }

            let (channels, _bytes_dl) =
                get_object_up_to_layer(&self.gf, &manifest, &live, max_layer)
                    .await
                    .map_err(|e| GatewayError::Decode(format!("embed decode: {e}")))?;
            if channels.len() < 3 || channels.iter().any(|c| c.len() != n) {
                return Err(GatewayError::Decode(
                    "embed decode: unexpected channel shape".into(),
                ));
            }
            let mut rgb = vec![0u8; 3 * n];
            for i in 0..n {
                rgb[3 * i] = channels[0][i].clamp(0.0, 255.0).round() as u8;
                rgb[3 * i + 1] = channels[1][i].clamp(0.0, 255.0).round() as u8;
                rgb[3 * i + 2] = channels[2][i].clamp(0.0, 255.0).round() as u8;
            }
            let name_owned = name.to_string();
            let embedder_h = Arc::clone(&embedder);
            let idx_path = index_path.clone();
            tokio::task::spawn_blocking(move || -> Result<(), GatewayError> {
                let vec = embedder_h
                    .embed_image(&rgb, width, height)
                    .map_err(|e| GatewayError::Decode(format!("clip image: {e}")))?;
                let idx = holofs_embed::Index::open(&idx_path)
                    .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
                let rec = holofs_embed::EmbedRecord {
                    data_cid,
                    band,
                    name: name_owned,
                    vec,
                };
                idx.append(&rec)
                    .map_err(|e| GatewayError::BadRequest(format!("embed append: {e}")))?;
                Ok(())
            })
            .await
            .map_err(|e| GatewayError::BadRequest(format!("embed join: {e}")))??;
            any_new = true;
        }
        if any_new {
            // Stage 14.2: bump the ANN generation so the next
            // semantic_search call rebuilds (or — for small bands —
            // re-loads the in-memory record vec). The rebuild itself
            // is lazy; we just signal staleness here.
            self.embed.lock().await.ann_generation += 1;
        }
        Ok(any_new)
    }

    /// Semantic search backed by [`holofs_embed::HnswIndex`].
    ///
    /// Stage 14.2: previously a brute-force flat scan over
    /// `embeddings.bin` on every query. Now lazily builds an
    /// in-memory ANN index, cached across queries until the next
    /// PUT bumps `ann_generation`. Small bands still fall back to
    /// brute-force inside the index (cheap and lower-latency under
    /// a few hundred vectors); larger bands graduate to HNSW. The
    /// public contract is identical — same SemanticHit shape, same
    /// "best-band-per-name when band == Any" semantics.
    pub async fn semantic_search(
        &self,
        query: &str,
        limit: usize,
        band: SearchBand,
    ) -> Result<Vec<SemanticHit>, GatewayError> {
        let Some(embedder) = self.ensure_embedder().await? else {
            return Ok(Vec::new());
        };
        let ann = match self.ensure_ann_index().await? {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };
        let query = query.to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<SemanticHit>, GatewayError> {
            let q = embedder
                .embed_text(&query)
                .map_err(|e| GatewayError::Decode(format!("clip text: {e}")))?;
            let raw = match band {
                SearchBand::Any => ann.search_any(&q, limit),
                other => {
                    let band_enum = match other {
                        SearchBand::Coarse => holofs_embed::LayerBand::Coarse,
                        SearchBand::Mid => holofs_embed::LayerBand::Mid,
                        SearchBand::Full => holofs_embed::LayerBand::Full,
                        SearchBand::Any => unreachable!(),
                    };
                    ann.search(band_enum, &q, limit)
                }
            };
            let hits: Vec<SemanticHit> = raw
                .into_iter()
                .map(|h| SemanticHit {
                    name: h.name,
                    score: h.score,
                    band: match h.band {
                        holofs_embed::LayerBand::Coarse => SearchBand::Coarse,
                        holofs_embed::LayerBand::Mid => SearchBand::Mid,
                        holofs_embed::LayerBand::Full => SearchBand::Full,
                    },
                })
                .collect();
            Ok(hits)
        })
        .await
        .map_err(|e| GatewayError::BadRequest(format!("search join: {e}")))?;
        result
    }

    /// Lazily build (or reuse) the in-memory ANN index. Rebuilds the
    /// whole index when the cached generation is stale relative to
    /// `ann_generation`; otherwise returns the cached `Arc` directly.
    /// Returns `Ok(None)` when the embed feature is disabled or the
    /// `embeddings.bin` path is unset.
    async fn ensure_ann_index(
        &self,
    ) -> Result<Option<Arc<holofs_embed::HnswIndex>>, GatewayError> {
        // Fast path.
        {
            let s = self.embed.lock().await;
            if !s.enabled {
                return Ok(None);
            }
            if let (Some(ann), Some(built_at)) = (&s.ann, s.ann_built_at) {
                if built_at == s.ann_generation {
                    return Ok(Some(Arc::clone(ann)));
                }
            }
        }
        // Slow path — rebuild. Snapshot path + generation, drop the
        // lock, read records, build HNSW off-thread, then store back.
        let (index_path, generation) = {
            let s = self.embed.lock().await;
            let p = match &s.index_path {
                Some(p) => p.clone(),
                None => return Ok(None),
            };
            (p, s.ann_generation)
        };
        let built = tokio::task::spawn_blocking(move || -> Result<holofs_embed::HnswIndex, GatewayError> {
            let idx = holofs_embed::Index::open(&index_path)
                .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
            let mut recs: Vec<holofs_embed::EmbedRecord> = Vec::new();
            for r in idx
                .iter()
                .map_err(|e| GatewayError::BadRequest(format!("embed iter: {e}")))?
            {
                let r = r.map_err(|e| GatewayError::BadRequest(format!("embed rec: {e}")))?;
                if !r.vec.is_empty() {
                    recs.push(r);
                }
            }
            Ok(holofs_embed::HnswIndex::build_from(recs))
        })
        .await
        .map_err(|e| GatewayError::BadRequest(format!("ann build join: {e}")))??;
        let arc = Arc::new(built);
        let mut s = self.embed.lock().await;
        // Another task may have raced us with a *newer* generation;
        // if so we still write ours — the next query will rebuild
        // again, which is fine. The point of the cache is amortising
        // across many queries between writes, not strict freshness.
        if s.ann_generation == generation {
            s.ann = Some(Arc::clone(&arc));
            s.ann_built_at = Some(generation);
        }
        Ok(Some(arc))
    }

    /// Fire-and-forget embedding for a freshly-PUT object. Called
    /// from the web / MCP ingest handlers right after `ingest_bytes`
    /// succeeds. Returns immediately; failures land in stderr. No-op
    /// when the embed feature is disabled.
    pub fn embed_object_in_background(self: &Arc<Self>, name: String) {
        let gw = Arc::clone(self);
        tokio::spawn(async move {
            if !gw.embed_enabled().await {
                return;
            }
            if let Err(e) = gw.embed_object(&name).await {
                eprintln!("embed bg {name}: {e}");
            }
        });
    }

    /// Stage 13.2: holographic spotlight — decode the image twice (L0
    /// only and full quality), then composite per-pixel so the rectangle
    /// `(x_pct, y_pct, w_pct, h_pct)` inside the image is sharp while
    /// everything else stays at L0 blur. The architectural pitch from
    /// `/about` made concrete: detail layers selectively rendered to
    /// the spatial region the user cares about, no re-encoding of
    /// anything.
    ///
    /// `roi` is normalised: each coordinate is `0.0..=1.0` of the image
    /// width / height. Clamped to image bounds.
    pub async fn spotlight(
        &self,
        name: &str,
        roi: SpotlightRoi,
    ) -> Result<SpotlightImage, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "spotlight: not applicable to non-image objects".into(),
            ));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let max_layer = manifest.nlayers.saturating_sub(1);

        // Coarse pass (L0). Cheap — typically a tenth of the bytes of
        // the full decode.
        let (lo_channels, lo_bytes) =
            get_object_up_to_layer(&self.gf, &manifest, &live, 0)
                .await
                .map_err(|e| GatewayError::Decode(format!("spotlight L0: {e}")))?;
        // Full pass.
        let (hi_channels, hi_bytes) =
            get_object_up_to_layer(&self.gf, &manifest, &live, max_layer)
                .await
                .map_err(|e| GatewayError::Decode(format!("spotlight full: {e}")))?;

        let w = manifest.width as usize;
        let h = manifest.height as usize;
        let n_pixels = w * h;
        if lo_channels.len() != hi_channels.len()
            || lo_channels.iter().any(|c| c.len() != n_pixels)
            || hi_channels.iter().any(|c| c.len() != n_pixels)
        {
            return Err(GatewayError::Decode(
                "spotlight: channel shape mismatch".into(),
            ));
        }
        // Translate normalised ROI to pixel coords, clamped to image
        // bounds. `roi.w == 0 || roi.h == 0` produces an all-blurry
        // image — same behaviour as "no spotlight requested".
        let x0 = (roi.x.clamp(0.0, 1.0) * w as f32).round() as usize;
        let y0 = (roi.y.clamp(0.0, 1.0) * h as f32).round() as usize;
        let x1 = ((roi.x + roi.w).clamp(0.0, 1.0) * w as f32).round() as usize;
        let y1 = ((roi.y + roi.h).clamp(0.0, 1.0) * h as f32).round() as usize;
        let n_channels = lo_channels.len();
        let mut composed: Vec<Vec<f32>> = vec![Vec::with_capacity(n_pixels); n_channels];
        for c in 0..n_channels {
            composed[c].resize(n_pixels, 0.0);
            for y in 0..h {
                let inside_y = y >= y0 && y < y1;
                let row_off = y * w;
                for x in 0..w {
                    let inside = inside_y && x >= x0 && x < x1;
                    let idx = row_off + x;
                    composed[c][idx] = if inside {
                        hi_channels[c][idx]
                    } else {
                        lo_channels[c][idx]
                    };
                }
            }
        }
        let png = encode_png(&composed, manifest.width, manifest.height);
        Ok(SpotlightImage {
            bytes: png,
            width: manifest.width,
            height: manifest.height,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            bytes_downloaded: lo_bytes + hi_bytes,
            decode_ms: t0.elapsed().as_millis(),
            roi_px: (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32),
        })
    }

    /// Stage 14.1: "coefficient-mask" spotlight — alternative to
    /// [`Self::spotlight`]'s spatial composite.
    ///
    /// Maps the spatial ROI to the set of DWT-plane positions whose
    /// coefficient affects ROI pixels (via the Haar reverse map in
    /// `holofs_core::transform`), then decodes the whole image but
    /// places only those coefficients into the reconstruction plane
    /// before the inverse Haar. Non-ROI pixels collapse to black.
    ///
    /// Visual difference vs Stage 13.2:
    ///   * Stage 13.2 (`spotlight`) = decode coarse + full, composite
    ///     per pixel. Outside ROI stays blurry-but-visible.
    ///   * Stage 14.1 (`spotlight_coeff`) = decode every layer, mask
    ///     coefficients outside ROI. Outside ROI is black (or near-
    ///     black, since Haar with masked high coefficients leaks a
    ///     little).
    ///
    /// Same bandwidth as a full fetch — RLNC requires the whole
    /// layer's shards to decode any coefficient. The win is purely
    /// in the spatial primitive: this is "operate on shard-level
    /// coefficients", made visible.
    pub async fn spotlight_coeff(
        &self,
        name: &str,
        roi: SpotlightRoi,
    ) -> Result<SpotlightImage, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "spotlight_coeff: not applicable to non-image objects".into(),
            ));
        }
        let w = manifest.width as usize;
        let h = manifest.height as usize;
        let levels = manifest.levels as usize;
        let x0 = (roi.x.clamp(0.0, 1.0) * w as f32).round() as usize;
        let y0 = (roi.y.clamp(0.0, 1.0) * h as f32).round() as usize;
        let x1 = ((roi.x + roi.w).clamp(0.0, 1.0) * w as f32).round() as usize;
        let y1 = ((roi.y + roi.h).clamp(0.0, 1.0) * h as f32).round() as usize;
        if x1 <= x0 || y1 <= y0 {
            return Err(GatewayError::BadRequest(
                "spotlight_coeff: empty ROI".into(),
            ));
        }
        let positions = holofs_core::transform::spatial_to_dwt_positions(
            x0,
            y0,
            x1 - x0,
            y1 - y0,
            w,
            h,
            levels,
        );
        let allowed: std::collections::HashSet<usize> = positions.into_iter().collect();

        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) =
            get_object_with_coeff_mask(&self.gf, &manifest, &live, &allowed)
                .await
                .map_err(|e| GatewayError::Decode(format!("spotlight_coeff: {e}")))?;
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, manifest.width, manifest.height);
        Ok(SpotlightImage {
            bytes: png,
            width: manifest.width,
            height: manifest.height,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            bytes_downloaded: bytes_dl,
            decode_ms,
            roi_px: (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32),
        })
    }

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

// === Stage 13.4: per-object version history ================================

/// Lazy versioning state. `enabled` set via `Gateway::enable_versions`;
/// the on-disk layout is `root/<sanitized_name>/v<ts_ms>_<cid8>.bin`,
/// each file holding one `Manifest::encode()` of a prior version.
#[derive(Default)]
struct VersionsState {
    enabled: bool,
    root: Option<std::path::PathBuf>,
}

/// One row of [`Gateway::list_versions`].
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Opaque id used in restore — the filename minus `.bin`. URL-safe.
    pub id: String,
    /// Unix ms when the version was archived (== time the *next* PUT
    /// for this name landed). Drives the human-readable timestamp.
    pub created_at_ms: u64,
    /// First 16 hex chars of the manifest's `data_cid`. Lets the user
    /// confirm a version is the one they're looking for without
    /// scrolling the whole hash.
    pub cid_short: String,
    /// `width × height` for image / `samples × 1` for audio — same
    /// rendering as on the catalog row.
    pub width: u32,
    pub height: u32,
    /// Kind so the UI can pick the right thumbnail strategy.
    pub kind: ObjectKind,
}

impl Gateway {
    /// Path to the directory holding version side files for `name`.
    /// Sanitisation: directory separators in the catalog name become
    /// `__` so each object gets a flat folder under `root`.
    fn version_dir_for(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let safe = name.replace('/', "__").replace(['\\', ':', '?', '*', '"', '<', '>', '|'], "_");
        root.join("versions").join(safe)
    }

    /// Sanitise / build the path for a single version file.
    fn version_file_for(
        root: &std::path::Path,
        name: &str,
        ts_ms: u64,
        cid: &[u8; 32],
    ) -> std::path::PathBuf {
        let cid_short: String = cid.iter().take(4).map(|b| format!("{b:02x}")).collect();
        Self::version_dir_for(root, name).join(format!("v{ts_ms}_{cid_short}.bin"))
    }

    /// Archive a manifest to the versions side store. Called from
    /// `ingest_bytes` BEFORE the catalog mutation and the shard purge
    /// (which we then skip for the prior shards). Cheap — just one
    /// encode + one file write.
    async fn archive_version(&self, name: &str, manifest: &Manifest) -> Result<(), GatewayError> {
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Ok(());
        };
        drop(s);
        let dir = Self::version_dir_for(&root, name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| GatewayError::BadRequest(format!("versions mkdir: {e}")))?;
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let path = Self::version_file_for(&root, name, ts_ms, &manifest.data_cid);
        let bytes = manifest.encode();
        std::fs::write(&path, &bytes)
            .map_err(|e| GatewayError::BadRequest(format!("versions write: {e}")))?;
        Ok(())
    }

    /// List archived versions of `name`, newest first. Returns an
    /// empty vec when versioning is off or no versions exist.
    pub async fn list_versions(
        &self,
        name: &str,
    ) -> Result<Vec<VersionEntry>, GatewayError> {
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Ok(Vec::new());
        };
        drop(s);
        let dir = Self::version_dir_for(&root, name);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<VersionEntry> = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| GatewayError::BadRequest(format!("versions readdir: {e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // `vTS_CID.bin` → parse.
            let stem = match file_name.strip_suffix(".bin") {
                Some(s) => s,
                None => continue,
            };
            let after_v = match stem.strip_prefix('v') {
                Some(s) => s,
                None => continue,
            };
            let (ts, cid_short) = match after_v.split_once('_') {
                Some((ts, cid)) => (ts.parse::<u64>().unwrap_or(0), cid.to_string()),
                None => continue,
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let manifest = match Manifest::decode(&bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(VersionEntry {
                id: stem.to_string(),
                created_at_ms: ts,
                cid_short,
                width: manifest.width,
                height: manifest.height,
                kind: manifest.kind,
            });
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at_ms));
        Ok(out)
    }

    /// Swap the catalog entry for `name` with the archived version
    /// `id`. The currently-live manifest is archived first so the
    /// swap is reversible (it appears as a fresh version with the
    /// current timestamp). Old shards stay on cluster nodes —
    /// versioning treats every version as immutable.
    pub async fn restore_version(
        &self,
        name: &str,
        id: &str,
    ) -> Result<RestoreResult, GatewayError> {
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Err(GatewayError::BadRequest(
                "versions disabled — restart with --enable-versions".into(),
            ));
        };
        drop(s);
        // Locate the version file by id.
        let dir = Self::version_dir_for(&root, name);
        let path = dir.join(format!("{id}.bin"));
        if !path.exists() {
            return Err(GatewayError::NotFound);
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| GatewayError::BadRequest(format!("versions read: {e}")))?;
        let target = Manifest::decode(&bytes)
            .map_err(|e| GatewayError::BadRequest(format!("versions decode: {e}")))?;

        // Archive the current manifest before replacing it. If the
        // name isn't in the catalog at all (was deleted), restore
        // becomes a pure resurrection — no prior to archive.
        let current = self.catalog.lock().await.get(name).cloned();
        if let Some(prev) = &current {
            self.archive_version(name, prev).await?;
        }

        let restored_cid = hex(&target.data_cid);
        self.catalog
            .lock()
            .await
            .insert(name.to_string(), target);
        self.invalidate_cache(name).await;
        self.persist_catalog().await;
        Ok(RestoreResult {
            name: name.to_string(),
            restored_cid_hex: restored_cid,
        })
    }
}

/// Result of [`Gateway::restore_version`].
#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub name: String,
    /// Hex `data_cid` of the now-live manifest after the swap.
    pub restored_cid_hex: String,
}

/// Stage 14.0: per-node breakdown of one GC pass.
#[derive(Debug, Clone)]
pub struct GcNodeReport {
    /// `idx` inside `cluster.node_addrs`.
    pub node_idx: usize,
    /// Address the gateway used to reach it.
    pub node_addr: String,
    /// Total shard hashes the node reported holding (before purge).
    pub held: u64,
    /// Shard hashes that were not referenced by any live manifest or
    /// version archive — these were purged.
    pub orphaned: u64,
    /// `true` when the node responded to both `ListHashes` and
    /// `PurgeByHash`. `false` on RPC errors — the node is then
    /// reported with zero counts and a non-empty `error` field.
    pub ok: bool,
    pub error: Option<String>,
}

/// Result of [`Gateway::gc_orphaned_shards`].
#[derive(Debug, Clone)]
pub struct GcReport {
    /// Distinct shard hashes referenced across the catalog + every
    /// version archive on disk. This is the protected set.
    pub live_hashes: u64,
    /// Manifests scanned (catalog + versions combined).
    pub manifests_scanned: u64,
    /// Total shards across the cluster before the purge step.
    pub held_total: u64,
    /// Sum of `orphaned` across nodes — how many shards got purged.
    pub purged_total: u64,
    /// Per-node breakdown.
    pub nodes: Vec<GcNodeReport>,
    /// Stage 14.3: embedding records kept after rewriting
    /// embeddings.bin. `None` when the embed feature is off.
    pub embeddings_kept: Option<u64>,
    /// Stage 14.3: embedding records dropped (orphan data_cid +
    /// tombstones). `None` when the embed feature is off.
    pub embeddings_dropped: Option<u64>,
    /// Wall-clock duration in ms.
    pub duration_ms: u128,
}

impl Gateway {
    /// Stage 14.0: garbage-collect orphan shards from every live
    /// cluster node.
    ///
    /// Live set = union of shard hashes referenced by:
    ///   * every manifest currently in the catalog,
    ///   * every archived manifest under `<storage>/versions/*/v*.bin`
    ///     (so `restore_version` keeps working).
    ///
    /// Held set = `ListHashes` from each node. Orphans = held - live.
    /// One `PurgeByHash` round per node deletes the orphans.
    ///
    /// **Concurrency note:** a brief catalog lock snapshots names +
    /// manifests, then the lock is dropped. PUTs during the GC pass
    /// land fine — their shards go to the cluster after our `ListHashes`
    /// has already enumerated, so they're not in the held set we
    /// purge against. The catch is a PUT-then-REPLACE that lands
    /// **between** ListHashes and PurgeByHash on the same node: the
    /// old shard is on the held list, was not in the snapshot's live
    /// set, gets purged. Versioning archives the prior manifest
    /// before the catalog mutation though, so live set rebuilt
    /// next pass picks the old shards back up — and the next PUT
    /// pass will recreate them via the standard repair pipeline if
    /// the user calls `restore_version`. Documented gap.
    ///
    /// Returns a [`GcReport`] with per-node breakdown.
    pub async fn gc_orphaned_shards(&self) -> Result<GcReport, GatewayError> {
        use std::collections::HashSet;
        let t0 = Instant::now();

        // 1. Snapshot the live catalog hashes.
        //
        // Stage 14.3: alongside the shard-hash set we also build the
        // set of live `data_cid`s — used at the end of the pass to
        // tombstone embeddings whose owning object no longer exists
        // anywhere (catalog + version archives).
        let mut live: HashSet<[u8; 32]> = HashSet::new();
        let mut live_cids: HashSet<[u8; 32]> = HashSet::new();
        let mut manifests_scanned: u64 = 0;
        {
            let cat = self.catalog.lock().await;
            for (_, m) in cat.entries.iter() {
                if m.kind == ObjectKind::Directory {
                    continue;
                }
                manifests_scanned += 1;
                live_cids.insert(m.data_cid);
                for chan in &m.shard_hashes {
                    for per_l in chan {
                        for h in per_l {
                            live.insert(*h);
                        }
                    }
                }
            }
        }

        // 2. Walk the on-disk version archive directory and merge
        //    those manifests' shard hashes into the live set so a
        //    `restore_version` after GC still finds its shards intact.
        let versions_dir = self
            .versions
            .lock()
            .await
            .root
            .clone()
            .map(|r| r.join("versions"));
        if let Some(dir) = versions_dir {
            if dir.exists() {
                if let Ok(by_name) = std::fs::read_dir(&dir) {
                    for name_entry in by_name.flatten() {
                        let path = name_entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        if let Ok(versions) = std::fs::read_dir(&path) {
                            for v_entry in versions.flatten() {
                                let p = v_entry.path();
                                if p.extension().and_then(|s| s.to_str()) != Some("bin") {
                                    continue;
                                }
                                let bytes = match std::fs::read(&p) {
                                    Ok(b) => b,
                                    Err(_) => continue,
                                };
                                let m = match Manifest::decode(&bytes) {
                                    Ok(m) => m,
                                    Err(_) => continue,
                                };
                                manifests_scanned += 1;
                                live_cids.insert(m.data_cid);
                                for chan in &m.shard_hashes {
                                    for per_l in chan {
                                        for h in per_l {
                                            live.insert(*h);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let live_count = live.len() as u64;

        // 3. For each live node, ListHashes → compute orphans → PurgeByHash.
        let live_nodes = self.effective_live().await;
        let mut nodes: Vec<GcNodeReport> = Vec::with_capacity(live_nodes.len());
        let mut held_total: u64 = 0;
        let mut purged_total: u64 = 0;
        for node_idx in live_nodes {
            let addr = self.cluster.node_addrs[node_idx].clone();
            let held = match holofs_client::list_node_hashes(&addr).await {
                Ok(h) => h,
                Err(e) => {
                    nodes.push(GcNodeReport {
                        node_idx,
                        node_addr: addr,
                        held: 0,
                        orphaned: 0,
                        ok: false,
                        error: Some(e.to_string()),
                    });
                    continue;
                }
            };
            let held_n = held.len() as u64;
            held_total += held_n;
            let orphans: Vec<[u8; 32]> = held
                .into_iter()
                .filter(|h| !live.contains(h))
                .collect();
            let orphan_n = orphans.len() as u64;
            let (ok, err) = if orphans.is_empty() {
                (true, None)
            } else {
                match holofs_client::purge_node_by_hash(&addr, orphans).await {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                }
            };
            if ok {
                purged_total += orphan_n;
            }
            nodes.push(GcNodeReport {
                node_idx,
                node_addr: addr,
                held: held_n,
                orphaned: orphan_n,
                ok,
                error: err,
            });
        }

        // Stage 14.3: embedding GC — rewrite embeddings.bin keeping
        // only records whose data_cid is still in `live_cids`. Also
        // strips tombstones for free (rewrite_keep drops empty-vec
        // records unconditionally). Bumps ann_generation so the next
        // semantic_search rebuilds the in-memory ANN index without
        // stale hits.
        let (emb_kept, emb_dropped) = {
            let state = self.embed.lock().await;
            if !state.enabled {
                (None, None)
            } else {
                match state.index_path.clone() {
                    None => (None, None),
                    Some(path) => {
                        drop(state);
                        // Off-thread because the rewrite walks the
                        // whole file and we don't want to block the
                        // tokio runtime on disk IO.
                        let cids = live_cids.clone();
                        let path_for_task = path.clone();
                        let result = tokio::task::spawn_blocking(
                            move || -> Result<(usize, usize), GatewayError> {
                                let idx = holofs_embed::Index::open(&path_for_task)
                                    .map_err(|e| {
                                        GatewayError::BadRequest(format!(
                                            "embed index: {e}"
                                        ))
                                    })?;
                                idx.rewrite_keep(|cid| cids.contains(cid))
                                    .map_err(|e| {
                                        GatewayError::BadRequest(format!(
                                            "embed rewrite: {e}"
                                        ))
                                    })
                            },
                        )
                        .await
                        .map_err(|e| {
                            GatewayError::BadRequest(format!("embed gc join: {e}"))
                        })??;
                        // Invalidate the ANN cache so the next search
                        // rebuilds against the rewritten file.
                        let mut s = self.embed.lock().await;
                        s.ann_generation += 1;
                        s.ann = None;
                        s.ann_built_at = None;
                        (Some(result.0 as u64), Some(result.1 as u64))
                    }
                }
            }
        };

        Ok(GcReport {
            live_hashes: live_count,
            manifests_scanned,
            held_total,
            purged_total,
            nodes,
            embeddings_kept: emb_kept,
            embeddings_dropped: emb_dropped,
            duration_ms: t0.elapsed().as_millis(),
        })
    }
}

/// Per-kind object counts inside [`ApiStats`].
#[derive(Debug, Default, Clone, Copy)]
pub struct KindCounts {
    /// Image (raster, DWT-encoded) objects.
    pub image: u64,
    /// Audio (1D Haar, WAV-encoded) objects.
    pub audio: u64,
    /// UTF-8 text objects (single-layer chunking).
    pub text: u64,
    /// Arbitrary binary blobs (single-layer erasure coded).
    pub opaque: u64,
    /// Directory markers (zero-byte tombstones; only path-resolution metadata).
    pub directory: u64,
}

/// Snapshot returned by [`Gateway::api_stats`].
#[derive(Debug, Clone)]
pub struct ApiStats {
    /// Total nodes the gateway knows about (including admin-killed).
    pub nodes_total: usize,
    /// Nodes that are not currently admin-killed.
    pub nodes_live: usize,
    /// Object count in the catalog.
    pub objects_total: usize,
    /// Breakdown of `objects_total` by `ObjectKind`.
    pub objects_by_kind: KindCounts,
    /// Total shards planned across every (channel, layer) of every object.
    pub shards_total: u64,
    /// Unique shard hashes — `< shards_total` iff dedup kicked in.
    pub shards_unique: u64,
    /// `(1 - unique/total) * 100` rounded to two decimals.
    pub dedup_savings_pct: f64,
    /// Approximate stored bytes across the cluster (sum of `n * (K + sym_len)`).
    pub bytes_total: u64,
}

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

/// One row of the per-node table shown on `/health`.
#[derive(Debug, Clone)]
pub struct NodeStatus {
    /// Cluster-wide index (matches `ClusterInfo::node_addrs`).
    pub idx: usize,
    /// `host:port` address.
    pub addr: String,
    /// Zone id (rack/AZ) for anti-affinity placement.
    pub zone: u8,
    /// `true` when the admin manually disabled this node from the UI.
    pub admin_killed: bool,
}

/// Snapshot returned by [`Gateway::health_index_data`].
#[derive(Debug, Clone)]
pub struct HealthIndexData {
    /// Per-node rows in `ClusterInfo::node_addrs` order.
    pub nodes: Vec<NodeStatus>,
    /// Names of every object in the catalog (sorted) — the page loads each
    /// object's health detail separately via [`Gateway::object_health`].
    pub objects: Vec<String>,
    /// Live count after admin-kill filtering.
    pub n_live: usize,
    /// Total node count.
    pub n_total: usize,
}

/// Result of toggling an admin-kill flag.
#[derive(Debug, Clone, Copy)]
pub struct AdminToggleResult {
    /// Zero-based node index.
    pub idx: usize,
    /// State after the toggle: `true` means the node is now admin-disabled.
    pub now_killed: bool,
}

// === Phase 4b.5: inspect / similar / diff view-models =====================

/// One shard placement inside [`LayerLayout`].
#[derive(Debug, Clone)]
pub struct ShardInfo {
    /// Shard index inside the (channel, layer) bucket.
    pub idx: u32,
    /// Node index in `Manifest::nodes` the shard currently maps to.
    pub node_idx: usize,
    /// `host:port` address of that node (empty if out of range).
    pub node_addr: String,
    /// `true` when `idx < k` — carries a raw chunk; otherwise RLNC.
    pub is_systematic: bool,
}

/// All shards belonging to one (channel, layer) pair.
#[derive(Debug, Clone)]
pub struct LayerLayout {
    /// Channel index (`0..channels`).
    pub channel: u8,
    /// Layer index (`0..nlayers`).
    pub layer: u8,
    /// Total shards in this (channel, layer).
    pub n_shards: u32,
    /// First `k` shards in `shards` are systematic.
    pub k_systematic: u16,
    /// One entry per shard, in `idx` order.
    pub shards: Vec<ShardInfo>,
}

/// Result of [`Gateway::inspect`] — drives the `/inspect/<name>` page.
#[derive(Debug, Clone)]
pub struct InspectInfo {
    /// Catalog name.
    pub name: String,
    /// Object kind — used to label channels/layers in the UI.
    pub kind: holofs_model::manifest::ObjectKind,
    /// Number of channels (R/G/B for image, L/R for audio, 1 for text/opaque).
    pub channels: u8,
    /// Total layer count in the manifest.
    pub nlayers: u8,
    /// Systematic-shard threshold.
    pub k: u16,
    /// One entry per (channel, layer), in row-major (channel, then layer) order.
    pub layers: Vec<LayerLayout>,
}

/// One verified shard fetched over the wire by [`Gateway::shard_payload`].
#[derive(Debug, Clone)]
pub struct ShardPayload {
    /// Shard payload bytes.
    pub payload: Vec<u8>,
    /// K-length coefficient vector (systematic shards = unit vector).
    pub coeffs: Vec<u8>,
    /// `true` when this is a systematic shard (raw chunk).
    pub is_systematic: bool,
    /// Lowercase hex of the shard's SHA-256.
    pub hash_hex: String,
    /// Node index in `Manifest::nodes` the shard currently maps to.
    pub node_idx: usize,
    /// `host:port` address of that node.
    pub node_addr: String,
    /// Symbol length for the parent layer.
    pub sym_len: u32,
}

/// Scope filter for [`Gateway::similar_to`]. Constrains the candidate
/// pool relative to the target object's parent directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarScope {
    /// Whole catalog (legacy default).
    All,
    /// Only direct siblings — entries whose parent dir equals the
    /// target's parent dir.
    Folder,
    /// Subtree — entries whose path starts at the target's parent dir.
    /// At the catalog root this collapses to `All`.
    Tree,
}

impl SimilarScope {
    /// Parse a query-string value (`all` / `folder` / `tree`). Unknown
    /// values fall back to `All` so a hand-edited URL can't break the page.
    pub fn parse(s: &str) -> Self {
        match s {
            "folder" => Self::Folder,
            "tree" => Self::Tree,
            _ => Self::All,
        }
    }
}

/// Directory portion of a catalog name. `"a/b/c.png"` → `"a/b"`;
/// `"top.png"` → `""`.
pub(crate) fn parent_dir(name: &str) -> &str {
    name.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Whether `candidate` is in the comparison pool for a target whose
/// parent directory is `target_parent`, under the given `scope`.
pub(crate) fn in_scope(target_parent: &str, candidate: &str, scope: SimilarScope) -> bool {
    match scope {
        SimilarScope::All => true,
        SimilarScope::Folder => parent_dir(candidate) == target_parent,
        SimilarScope::Tree => {
            if target_parent.is_empty() {
                // Target sits at the catalog root — tree scope spans the
                // whole catalog, indistinguishable from All.
                true
            } else {
                // Either inside target_parent directly or anywhere below it.
                candidate.starts_with(&format!("{target_parent}/"))
            }
        }
    }
}

/// Comparison method used in [`SimilarMatch::method`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarityMethod {
    /// MinHash Jaccard over 5-shingles (text).
    Jaccard,
    /// Hamming distance over the 45-bit dHash derived from the per-channel
    /// 48-byte perceptual fingerprint (image/audio).
    DHash,
}

/// One row of the top-similar table on `/similar/<name>`.
#[derive(Debug, Clone)]
pub struct SimilarMatch {
    /// Catalog name of the matched object.
    pub name: String,
    /// Percentage in `[0.0, 100.0]`.
    pub similarity_pct: f32,
    /// Comparison method that produced `similarity_pct`.
    pub method: SimilarityMethod,
}

/// One row of the shard-overlap table on `/similar/<name>`.
#[derive(Debug, Clone)]
pub struct ShardOverlap {
    /// Catalog name of the other object.
    pub name: String,
    /// Count of shard hashes shared with the target object.
    pub common: usize,
    /// `common / total_target * 100`.
    pub overlap_pct: f32,
    /// Stage 13.0: percentage of *low-layer* (structure / silhouette)
    /// shards of the target that this neighbour also carries. Computed
    /// over the bottom half of layers — for the typical `nlayers=8`
    /// image that's L0..=L3.
    pub low_layer_overlap_pct: f32,
    /// Percentage of *high-layer* (detail / texture) shards shared.
    /// Top half of layers.
    pub high_layer_overlap_pct: f32,
    /// `low - high` percentage points. Positive values flag "robust
    /// copies": files where the structure is preserved (low layers
    /// hash-identical) but detail differs — exactly what a watermark,
    /// recompression, or light retouch produces.
    pub robust_copy_score: f32,
}

/// Result of [`Gateway::similar_to`].
#[derive(Debug, Clone)]
pub struct SimilarReport {
    /// Target catalog name (the object the page is rendered for).
    pub name: String,
    /// Target object kind — drives the methodology blurb.
    pub kind: holofs_model::manifest::ObjectKind,
    /// Hex preview of the comparison fingerprint (MinHash for text, FP for media).
    pub fingerprint_hex: String,
    /// Length of the MinHash sketch (= [`holofs_analytics::shingle::MINHASH_K`]).
    pub minhash_k: usize,
    /// Total shard count of the target object (denominator for overlap %).
    pub total_shards: usize,
    /// Top-10 neighbours of the same kind, sorted by similarity desc.
    pub neighbors: Vec<SimilarMatch>,
    /// Cross-object shard-hash overlaps (any kind), sorted by `common` desc.
    pub overlaps: Vec<ShardOverlap>,
}

/// One coloured cell of the `/diff/<a>/<b>` grid.
#[derive(Debug, Clone, Copy)]
pub struct DiffCell {
    /// Shard index inside the (channel, layer) bucket.
    pub idx: u32,
    /// `true` when the chunk-hash matches between both objects.
    pub is_common: bool,
}

/// One row of the per-layer chunk-diff grid.
#[derive(Debug, Clone)]
pub struct DiffLayer {
    /// Channel index.
    pub channel: u8,
    /// Layer index.
    pub layer: u8,
    /// Cells marked `is_common`.
    pub n_common: usize,
    /// Total cells in the row.
    pub n_total: usize,
    /// One cell per systematic chunk.
    pub cells: Vec<DiffCell>,
}

/// Result of [`Gateway::diff_chunks`].
#[derive(Debug, Clone)]
pub struct DiffReport {
    /// Catalog name of object A.
    pub name_a: String,
    /// Catalog name of object B.
    pub name_b: String,
    /// Common object kind (diff is rejected when A and B kinds differ).
    pub kind: holofs_model::manifest::ObjectKind,
    /// Total common chunks across all layers.
    pub common: usize,
    /// Total compared chunks (across all `min(channels, layers, k)` cells).
    pub total: usize,
    /// `common / total * 100`.
    pub similarity_pct: f32,
    /// `common * sym_len[0]` — approximate bytes saved by dedup.
    pub storage_saved_bytes: u64,
    /// One entry per (channel, layer), in `BTreeMap` order.
    pub layers: Vec<DiffLayer>,
}

/// Result of [`Gateway::mix_objects`] — the freshly assembled hybrid
/// PNG plus accounting fields for the caller to log / display. The
/// bytes can be streamed back to the user as-is or pushed back into
/// the catalog via [`Gateway::ingest_bytes`] for a "save as" flow.
#[derive(Debug, Clone)]
pub struct MixedImage {
    /// PNG bytes of the hybrid image.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    /// Split layer used to partition layers between the two sources.
    /// Layers `0..=split` came from `name_a`, layers `>split` from
    /// `name_b`.
    pub split_layer: u8,
    /// Total number of layers either object has (validated equal).
    pub nlayers: u8,
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
}

/// Result of [`Gateway::filter_audio`] — the freshly rendered WAV
/// plus the list of layers that actually contributed. Layers not in
/// the list were zero-filled before the inverse Haar.
#[derive(Debug, Clone)]
pub struct FilteredAudio {
    /// 16-bit PCM WAV bytes.
    pub bytes: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
    pub nlayers: u8,
    /// Indices of layers whose coefficients were preserved.
    pub kept_layers: Vec<u8>,
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
}

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

/// Stage 13.2: region-of-interest for [`Gateway::spotlight`]. All four
/// coordinates are normalised image-relative (`0.0..=1.0`). `x`/`y` is
/// the top-left corner; `w`/`h` is the rectangle's extent.
#[derive(Debug, Clone, Copy)]
pub struct SpotlightRoi {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Result of [`Gateway::spotlight`].
#[derive(Debug, Clone)]
pub struct SpotlightImage {
    /// PNG bytes of the composited image: full-quality inside the ROI,
    /// L0 (coarse) blur outside.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub nlayers: u8,
    /// Sum of both decode passes' bandwidth.
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
    /// `(x, y, w, h)` of the ROI in actual pixels after clamping —
    /// echoed back so the UI can draw a frame on top of the image.
    pub roi_px: (u32, u32, u32, u32),
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

/// One row of [`EscrowSplitResult::shares`] — describes a `.holoshare` file
/// the frontend should offer as a download link.
#[derive(Debug, Clone)]
pub struct EscrowShareInfo {
    /// 0-based share index inside the `n`-share set.
    pub idx: u16,
    /// `<id_hex>_<idx>.holoshare` URL stem; full URL is
    /// `/escrow/download/{download_path}`.
    pub download_path: String,
    /// Human-readable filename to offer to the user agent.
    pub filename: String,
    /// Size of the encoded `.holoshare` blob in bytes.
    pub bytes: usize,
}

/// Result of [`Gateway::escrow_split`].
#[derive(Debug, Clone)]
pub struct EscrowSplitResult {
    /// Original filename submitted by the user.
    pub filename: String,
    /// Raw input size in bytes.
    pub source_bytes: usize,
    /// Threshold (`k` of `n` needed to recover).
    pub k: usize,
    /// Total share count produced.
    pub n: usize,
    /// 32-char hex of `escrow_id` (first 16 bytes of SHA-256 over the source).
    pub escrow_id_hex: String,
    /// One entry per produced share, in index order.
    pub shares: Vec<EscrowShareInfo>,
}

/// Result of [`Gateway::escrow_download`] — encoded `.holoshare` bytes.
#[derive(Debug, Clone)]
pub struct EscrowShareBytes {
    /// 0-based share index inside the `n`-share set.
    pub idx: u16,
    /// `total_n` from the share metadata (for the `Content-Disposition` filename).
    pub total_n: u16,
    /// Encoded `.holoshare` payload.
    pub bytes: Vec<u8>,
}

/// Result of [`Gateway::escrow_recover`] — recovered file payload + metadata.
#[derive(Debug, Clone)]
pub struct EscrowRecoverResult {
    /// Decoded file bytes.
    pub data: Vec<u8>,
    /// MIME type the source was uploaded with (from the first share).
    pub content_type: String,
    /// Original filename for `Content-Disposition`.
    pub filename: String,
    /// Number of valid `.holoshare` files consumed.
    pub shares_used: usize,
}

impl Gateway {
    /// Split a file into `n` `.holoshare` shares with threshold `k`. Stores
    /// the in-memory share set keyed by `escrow_id` so that subsequent
    /// `/escrow/download/...` requests can hand them out.
    pub async fn escrow_split(
        &self,
        file_bytes: Vec<u8>,
        filename: String,
        k: usize,
        n: usize,
    ) -> Result<EscrowSplitResult, GatewayError> {
        if file_bytes.is_empty() {
            return Err(GatewayError::BadRequest("empty file".into()));
        }
        if !(1..=64).contains(&k) || !(k..=64).contains(&n) {
            return Err(GatewayError::BadRequest(
                "invalid K/N (1 ≤ K ≤ N ≤ 64)".into(),
            ));
        }
        let content_type = guess_opaque_content_type(&filename);
        let source_bytes = file_bytes.len();
        let params = holofs_analytics::escrow::EscrowParams {
            k,
            n,
            content_type,
            filename: filename.clone(),
        };
        let shares = holofs_analytics::escrow::split_into_shares(&file_bytes, &params);
        let escrow_id_hex = hex(&shares[0].escrow_id);
        let infos: Vec<EscrowShareInfo> = shares
            .iter()
            .enumerate()
            .map(|(i, sh)| EscrowShareInfo {
                idx: i as u16,
                download_path: format!("{escrow_id_hex}_{i}.holoshare"),
                filename: format!("share_{i:02}_of_{n}.holoshare"),
                bytes: sh.encode().len(),
            })
            .collect();
        self.escrow_cache
            .lock()
            .await
            .insert(escrow_id_hex.clone(), shares);
        Ok(EscrowSplitResult {
            filename,
            source_bytes,
            k,
            n,
            escrow_id_hex,
            shares: infos,
        })
    }

    /// Fetch one share by `escrow_id` hex + index. Returns `NotFound` when
    /// the gateway has been restarted (shares only live in RAM).
    pub async fn escrow_download(
        &self,
        escrow_id_hex: &str,
        idx: usize,
    ) -> Result<EscrowShareBytes, GatewayError> {
        let cache = self.escrow_cache.lock().await;
        let shares = cache.get(escrow_id_hex).ok_or_else(|| {
            GatewayError::NotFound
        })?;
        let share = shares
            .get(idx)
            .ok_or_else(|| GatewayError::BadRequest(format!("no share index {idx}")))?;
        Ok(EscrowShareBytes {
            idx: idx as u16,
            total_n: share.total_n,
            bytes: share.encode(),
        })
    }

    /// Recover the original file from a set of `.holoshare` blobs. The
    /// gateway does not need the cache for this — recovery is stateless and
    /// works as long as `k` valid shares from the same escrow are supplied.
    pub async fn escrow_recover(
        &self,
        share_blobs: Vec<Vec<u8>>,
    ) -> Result<EscrowRecoverResult, GatewayError> {
        let mut shares: Vec<holofs_analytics::escrow::ShareFile> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for (i, bytes) in share_blobs.into_iter().enumerate() {
            if bytes.is_empty() {
                continue;
            }
            match holofs_analytics::escrow::ShareFile::decode(&bytes) {
                Ok(sh) => shares.push(sh),
                Err(e) => errors.push(format!("share #{i}: {e}")),
            }
        }
        if shares.is_empty() {
            return Err(GatewayError::BadRequest(format!(
                "no valid .holoshare files ({})",
                errors.join("; ")
            )));
        }
        let shares_used = shares.len();
        let (data, content_type, filename) = holofs_analytics::escrow::recover_from_shares(&shares)
            .map_err(|e| GatewayError::BadRequest(format!("recover failed: {e}")))?;
        Ok(EscrowRecoverResult {
            data,
            content_type,
            filename,
            shares_used,
        })
    }
}

/// Guess the content-type of an arbitrary binary by extension. If unknown,
/// fall back to `application/octet-stream` (universal "untyped binary").
fn guess_opaque_content_type(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ppt" => "application/vnd.ms-powerpoint",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "gzip" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "epub" => "application/epub+zip",
        "mobi" => "application/x-mobipocket-ebook",
        "rtf" => "application/rtf",
        "sqlite" | "db" => "application/vnd.sqlite3",
        "exe" | "dll" => "application/vnd.microsoft.portable-executable",
        "dmg" => "application/x-apple-diskimage",
        "iso" => "application/x-iso9660-image",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Guess the content-type of a text file by name (for use on GET).
fn guess_text_content_type(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown; charset=utf-8".into()
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8".into()
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8".into()
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8".into()
    } else if lower.ends_with(".csv") {
        "text/csv; charset=utf-8".into()
    } else {
        "text/plain; charset=utf-8".into()
    }
}

fn encode_png(channels: &[Vec<f32>], w: u32, h: u32) -> Vec<u8> {
    let arr: [Vec<f32>; 3] = [
        channels[0].clone(),
        channels[1].clone(),
        channels[2].clone(),
    ];
    let rgb = to_rgb(&arr, w as usize, h as usize);
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&rgb).unwrap();
    }
    buf
}

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
