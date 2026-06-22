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

use holofs_client::{get_object_up_to_layer, purge_object, put_object, LiveNodes};
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
            gf,
            live,
            admin_kills: Arc::new(Mutex::new(vec![false; n])),
            cluster,
            cache: Mutex::new(HashMap::new()),
            shard_cache: Mutex::new(HashMap::new()),
            escrow_cache: Mutex::new(HashMap::new()),
        })
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
        let shards = holofs_client::gather_layer(manifest, &live, 0, 0)
            .await
            .unwrap_or_default();
        // Hash verification — we don't let garbage into the fingerprint.
        let expected: std::collections::HashSet<_> = manifest
            .shard_hashes
            .first()
            .and_then(|c| c.first())
            .map(|hs| hs.iter().copied().collect())
            .unwrap_or_default();
        let verified: Vec<holofs_core::rlnc::Shard> = shards
            .into_iter()
            .filter(|s| expected.contains(&holofs_core::merkle::shard_hash(s)))
            .collect();
        holofs_analytics::fingerprint::perceptual_fingerprint(manifest, &verified)
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
            if let Err(e) = purge_object(old, &live).await {
                eprintln!("PUT {name}: previous object failed to purge (continuing): {e}");
            }
        }
        let t0 = Instant::now();
        let (manifest, _kind_str, total_shards) = self
            .put_any(name, body, &live)
            .await
            .map_err(GatewayError::BadRequest)?;
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
        let manifest = Manifest::directory(directory_object_id(path));
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
    /// cross-object shard overlaps (any kind).
    pub async fn similar_to(&self, name: &str) -> Result<SimilarReport, GatewayError> {
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

        let mut neighbors: Vec<SimilarMatch> = Vec::new();
        for n in snapshot.names() {
            if n == name {
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
                (sim, SimilarityMethod::L1)
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
                overlaps.push(ShardOverlap {
                    name: n,
                    common,
                    overlap_pct: pct,
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

/// Comparison method used in [`SimilarMatch::method`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarityMethod {
    /// MinHash Jaccard over 5-shingles (text).
    Jaccard,
    /// L1 distance over the 16-byte perceptual fingerprint (image/audio).
    L1,
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
}
