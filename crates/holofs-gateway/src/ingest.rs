//! Universal PUT — auto-detect kind and stash bytes on the cluster.
//!
//! Entry point [`Gateway::ingest_bytes`] takes owned bytes + a
//! catalog name, tries image → audio → text → opaque in that order,
//! and either replaces the existing entry (archiving the prior
//! manifest first when `--enable-versions` is on) or writes fresh.
//!
//! The blank_* helpers build the pre-encoding manifest skeletons —
//! `put_object` / `put_audio_object` / `put_text_object` /
//! `put_opaque_object` from `holofs_client` fill in `data_cid`,
//! `merkle_root`, and `shard_hashes` in place.
//!

use std::sync::Arc;
use std::time::Instant;

use holofs_client::{put_object, put_object_replicated_blocks, LiveNodes};
use holofs_codec::image_io::load_photo_from_bytes;
use holofs_core::hash::hex;
use holofs_core::transform::coeff_layer;
use holofs_core::{K, LEVELS, NLAYERS, RED};
use holofs_model::manifest::{Manifest, ManifestState, ObjectEncoding, ObjectKind};
use holofs_model::path as catalog_path;

use crate::error::GatewayError;
use crate::util::{guess_opaque_content_type, guess_text_content_type, now_unix};
use crate::Gateway;

/// Lifecycle marker returned to the HTTP layer. `Ready` = the sync
/// path finished the encode + fanout; `Encoding` = the async path
/// staged a pending manifest and left the encode running in the
/// background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    Ready,
    Encoding,
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
    pub kind: ObjectKind,
    /// Total shards dispatched across the cluster.
    pub total_shards: u32,
    /// Ingestion wall-clock time in milliseconds. For async this is
    /// the *staging* time only — the background encode is not yet
    /// waited for.
    pub put_ms: u128,
    /// Whether the object is ready to serve immediately (`Ready`) or
    /// the client should poll for readiness (`Encoding`).
    pub outcome: IngestOutcome,
}

impl Gateway {
    /// Universal PUT: tries image → audio → text → opaque. Returns the
    /// finished Manifest (with shards already distributed), the kind
    /// label, and the total shard count. On failure the mutated
    /// manifest (if any) is surfaced in the error tuple's second slot
    /// so best-effort shard cleanup can use the *real*, data_cid-
    /// derived `object_id` instead of the placeholder that used to
    /// leak orphan shards until the next `/api/gc` (B11).
    pub(crate) async fn put_any(
        &self,
        name: &str,
        body: &[u8],
        live: &LiveNodes,
    ) -> Result<(Manifest, &'static str, u32), (String, Option<Manifest>)> {
        // 1. Image — the most common case, try first.
        if let Ok(arr) = load_photo_from_bytes(body, self.cluster.width, self.cluster.height) {
            let channels = vec![arr[0].clone(), arr[1].clone(), arr[2].clone()];
            let mut m = self.blank_manifest();
            if let Err(e) = put_object(&self.gf, &mut m, live, &channels).await {
                return Err((format!("put image: {e}"), Some(m)));
            }
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
                .map_err(|e| (format!("audio manifest: {e}"), None))?;
            if let Err(e) =
                holofs_client::put_audio_object(&self.gf, &mut m, live, &audio.channels).await
            {
                return Err((format!("put audio: {e}"), Some(m)));
            }
            let total: u32 = m.n_per_layer.iter().sum::<u32>() * m.channels as u32;
            return Ok((m, "audio", total));
        }
        // 3. Text — UTF-8 validation. If it fails → opaque.
        if let Ok(text) = std::str::from_utf8(body) {
            let ct = guess_text_content_type(name);
            let mut m = self.blank_text_manifest(ct);
            if let Err(e) = holofs_client::put_text_object(&self.gf, &mut m, live, text).await {
                return Err((format!("put text: {e}"), Some(m)));
            }
            let total = m.n_per_layer[0];
            return Ok((m, "text", total));
        }
        // 4. Opaque blob — last fallback. PDF, DOCX, ZIP, EXE, etc.
        let ct = guess_opaque_content_type(name);
        let mut m = self.blank_opaque_manifest(ct);
        if let Err(e) = holofs_client::put_opaque_object(&self.gf, &mut m, live, body).await {
            return Err((format!("put opaque: {e}"), Some(m)));
        }
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
            kind: ObjectKind::Opaque,
            content_type,
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
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
            kind: ObjectKind::Audio,
            content_type: "audio/wav".into(),
            chunk_lens: vec![],
            audio_sample_rate: sample_rate,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
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
            kind: ObjectKind::Text,
            content_type,
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
        }
    }

    /// Blank manifest for an image object. Width/height come from the cluster
    /// config. 3 channels (RGB), NLAYERS DWT bands.
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
            kind: ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
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
        // epoch-GC: no `gc_barrier` here anymore. Concurrent
        // GC snapshots its cutoff epoch before it starts and every
        // shard we're about to write gets a fresh (higher) epoch
        // from `Store::put`, so the node-side `PurgeByHashUpTo`
        // gate will refuse to purge our writes even if they land
        // between the GC's held-list snapshot and its purge RPC.
        if body.is_empty() {
            return Err(GatewayError::BadRequest("empty body".into()));
        }
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        // Reject before doing any expensive work: cannot write where a
        // directory already lives, and the parent directory must exist.
        {
            let cat = self.catalog.read().await;
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
        let prev = self.catalog.read().await.get(name).cloned();
        if let Some(old) = &prev {
            // when versioning is on we archive the prior
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
                // fix: the prior `purge_object(old, &live)`
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
        let (mut manifest, _kind_str, total_shards) = match self.put_any(name, body, &live).await {
            Ok(v) => v,
            Err((e, _partial)) => return Err(GatewayError::BadRequest(e)),
        };
        // stamp the manifest with creation time so the
        // tree view can sort by date.
        manifest.created_at_unix = now_unix();
        let put_ms = t0.elapsed().as_millis();
        let object_id = manifest.object_id;
        let cid_hex = hex(&manifest.data_cid);
        let kind = manifest.kind;
        let width = manifest.width;
        let height = manifest.height;
        // B10 CAS: the pre-check under read-lock at :309 saw an empty
        // slot or a non-directory. Between then and now a mkdir or
        // parallel PUT could have taken the slot; commit only if the
        // invariant still holds. On conflict purge the shards we
        // already dispatched — otherwise they become orphans until
        // /api/gc.
        {
            let mut cat = self.catalog.write().await;
            if let Some(existing) = cat.get(name) {
                if existing.kind == ObjectKind::Directory {
                    drop(cat);
                    if let Err(e) =
                        self.purge_orphans_of(&manifest, &live, Some(name)).await
                    {
                        eprintln!(
                            "PUT {name}: TOCTOU conflict purge failed (will settle at /api/gc): {e}"
                        );
                    }
                    return Err(GatewayError::AlreadyExists);
                }
            }
            cat.insert(name.to_string(), manifest);
        }
        self.invalidate_cache(name).await;
        self.persist_catalog().await?;
        Ok(IngestResult {
            name: name.to_string(),
            object_id,
            data_cid_hex: cid_hex,
            width,
            height,
            kind,
            total_shards,
            put_ms,
            outcome: IngestOutcome::Ready,
        })
    }

    /// image-only PUT that stores the object under the
    /// per-block Replicated encoding instead of the default RLNC.
    ///
    /// Semantically parallel to [`Self::ingest_bytes`] but:
    ///   * The kind auto-detect is restricted to image — audio /
    ///     text / opaque still ship via the RLNC path (they don't
    ///     benefit from per-block ROI addressing).
    ///   * Manifest `encoding` gets stamped as
    ///     [`ObjectEncoding::Replicated`] so downstream fetch paths
    ///     (spotlight-coeff, ROI decode) can pick the block-fetch
    ///     branch automatically.
    ///
    /// Every gc / versions / caching / persist step is identical to
    /// `ingest_bytes` — the split is only in the encode step.
    pub async fn ingest_bytes_replicated(
        &self,
        name: &str,
        body: &[u8],
        block_size: u32,
        replication: u8,
    ) -> Result<IngestResult, GatewayError> {
        if body.is_empty() {
            return Err(GatewayError::BadRequest("empty body".into()));
        }
        if block_size == 0 {
            return Err(GatewayError::BadRequest(
                "block_size must be >= 1".into(),
            ));
        }
        if replication == 0 {
            return Err(GatewayError::BadRequest(
                "replication must be >= 1".into(),
            ));
        }
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        {
            let cat = self.catalog.read().await;
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
        // Only images ride the replicated-block path — everything
        // else has no per-block ROI to save bandwidth on.
        let arr = load_photo_from_bytes(body, self.cluster.width, self.cluster.height)
            .map_err(|e| {
                GatewayError::BadRequest(format!(
                    "ingest_bytes_replicated: image decode failed \
                     (audio/text/opaque go through ingest_bytes): {e}"
                ))
            })?;
        let channels = vec![arr[0].clone(), arr[1].clone(), arr[2].clone()];
        let live = self.effective_live().await;
        if live.is_empty() {
            return Err(GatewayError::ClusterDegraded);
        }
        let prev = self.catalog.read().await.get(name).cloned();
        if let Some(old) = &prev {
            if self.versions_enabled().await {
                if let Err(e) = self.archive_version(name, old).await {
                    eprintln!("PUT {name}: version archive failed: {e}");
                }
            } else if let Err(e) = self.purge_orphans_of(old, &live, Some(name)).await {
                eprintln!("PUT {name}: previous object failed to purge (continuing): {e}");
            }
        }
        let t0 = Instant::now();
        let mut manifest = self.blank_manifest();
        put_object_replicated_blocks(&mut manifest, &live, &channels, block_size, replication)
            .await
            .map_err(|e| GatewayError::BadRequest(format!("put replicated: {e}")))?;
        manifest.created_at_unix = now_unix();
        let put_ms = t0.elapsed().as_millis();
        let total_shards: u32 = manifest.n_per_layer.iter().sum::<u32>()
            * manifest.channels as u32
            * replication as u32;
        let object_id = manifest.object_id;
        let cid_hex = hex(&manifest.data_cid);
        let kind = manifest.kind;
        let width = manifest.width;
        let height = manifest.height;
        self.catalog
            .write()
            .await
            .insert(name.to_string(), manifest);
        self.invalidate_cache(name).await;
        self.persist_catalog().await?;
        Ok(IngestResult {
            name: name.to_string(),
            object_id,
            data_cid_hex: cid_hex,
            width,
            height,
            kind,
            total_shards,
            put_ms,
            outcome: IngestOutcome::Ready,
        })
    }

    /// Async ingest — inserts a placeholder manifest with
    /// `state = Encoding` synchronously, spawns the encode + fanout
    /// on a detached tokio task, and returns immediately with a
    /// `Encoding` outcome. Read-side handlers gate on the state so a
    /// half-written object cannot be GET'd until the worker flips it
    /// to `Ready`.
    ///
    /// The `202 Accepted` return path is meant for burst-heavy soak
    /// workloads where the pre-async PUT p50 (~46 s under 50 workers
    /// on the 4-node topology) blew past the client's request
    /// timeout. Only images/audio/text/opaque under the default RLNC
    /// encoding go this way — the `?encoding=replicated` fork stays
    /// sync since ROI encode is bandwidth-heavy, not CPU-heavy.
    ///
    /// Failure model:
    /// - Validation errors (bad name, missing parent, empty body,
    ///   directory conflict, cluster degraded) return synchronously
    ///   before spawning anything.
    /// - Errors during background encode flip the manifest state to
    ///   `Failed` and bump `encode_failed_total`. Shards that made
    ///   it out are left for the next `/api/gc` sweep — MVP takes
    ///   the storage hit rather than reasoning about partial rollback.
    ///
    /// Idempotency:
    /// - Two concurrent PUTs to the same name are serialised on the
    ///   catalog mutex. Whichever loses the race sees an existing
    ///   `Encoding` manifest and 409s (the caller can retry once the
    ///   background job finishes). MVP behaviour; real-world clients
    ///   almost always send a single PUT then poll.
    pub async fn ingest_bytes_async(
        self: &Arc<Self>,
        name: &str,
        body: Vec<u8>,
    ) -> Result<IngestResult, GatewayError> {
        use std::sync::atomic::Ordering;

        if body.is_empty() {
            return Err(GatewayError::BadRequest("empty body".into()));
        }
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;

        // Intake backpressure. Without a ceiling, a bursty client
        // that isn't polling would let `objects_encoding` grow
        // linearly with request rate — the encoder can't drain
        // faster than one shard-fanout at a time, so pending
        // placeholders + their bodies would eventually OOM the
        // gateway. Refuse fast so the caller backs off (or its
        // retry loop pauses on the Retry-After header) instead.
        if self.objects_encoding.load(Ordering::Acquire) >= self.encode_queue_max as u64 {
            return Err(GatewayError::AsyncQueueFull);
        }

        // Same synchronous checks as `ingest_bytes` — reject before we
        // stage anything so the caller sees a clean 4xx rather than an
        // orphaned `Failed` manifest cluttering the catalog.
        {
            let cat = self.catalog.read().await;
            if let Some(existing) = cat.get(name) {
                if existing.kind == ObjectKind::Directory {
                    return Err(GatewayError::AlreadyExists);
                }
                if existing.state == ManifestState::Encoding {
                    // Another async PUT is still in flight for this
                    // name — refuse rather than double-writing. The
                    // dedicated `EncodingInProgress` variant lets
                    // the HTTP layer emit a Retry-After hint so
                    // the caller polls instead of retrying instantly.
                    return Err(GatewayError::EncodingInProgress);
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
        if live.is_empty() {
            return Err(GatewayError::ClusterDegraded);
        }

        // Compute the object-id up front so the pending manifest
        // has a stable identity clients can echo in polling requests.
        // The full data_cid is filled in by the encoding worker.
        use holofs_core::hash::Sha256;
        let mut hasher = Sha256::new();
        hasher.update(b"holofs-async-preview");
        hasher.update(name.as_bytes());
        hasher.update(&body);
        let preview = hasher.finalize();
        let object_id = u64::from_be_bytes(preview[0..8].try_into().unwrap());

        // Placeholder manifest — shape agnostic to detected kind.
        // The worker will replace it with the properly-encoded one
        // once auto-detect + encode + fanout finish.
        let placeholder = Manifest {
            object_id,
            k: K as u16,
            nlayers: 1,
            n_per_layer: vec![0],
            sym_len: vec![0],
            layer_positions: vec![vec![]],
            channels: 1,
            width: 0,
            height: 0,
            levels: 0,
            nodes: self.cluster.node_addrs.clone(),
            placement: self.cluster.placement,
            zones: self.cluster.zones.clone(),
            data_cid: preview,
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: ObjectKind::Opaque, // may be revised by worker
            content_type: "application/octet-stream".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: now_unix(),
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Encoding,
        };

        let t0 = Instant::now();
        // Handle prior object: archive-if-versions, else purge orphaned
        // shards. Do it before we insert the placeholder so orphan sweep
        // can't see two entries under the same name.
        let prev = self.catalog.read().await.get(name).cloned();
        if let Some(old) = &prev {
            if self.versions_enabled().await {
                if let Err(e) = self.archive_version(name, old).await {
                    eprintln!("PUT-async {name}: version archive failed: {e}");
                }
            } else if let Err(e) = self.purge_orphans_of(old, &live, Some(name)).await {
                eprintln!(
                    "PUT-async {name}: previous object failed to purge (continuing): {e}"
                );
            }
        }

        self.catalog
            .write()
            .await
            .insert(name.to_string(), placeholder);
        self.invalidate_cache(name).await;
        // NOTE: no persist_catalog here. The placeholder lives only in
        // RAM until the worker finalises; if we crash before the worker
        // finishes, the placeholder simply vanishes on reboot and the
        // caller re-PUTs (same recovery path bootstrap gives for
        // on-disk Encoding entries). Skipping the fsync here is the
        // single biggest fast-path win: under 50-worker soak load the
        // 202 p50 dropped from ~7.7 s → single digits of ms because
        // async PUTs no longer serialise on the catalog-persist fd.
        self.objects_encoding.fetch_add(1, Ordering::Relaxed);
        let staged_ms = t0.elapsed().as_millis();

        let gw = Arc::clone(self);
        let name_owned = name.to_string();
        tokio::spawn(async move {
            gw.run_encode_worker(name_owned, body).await;
        });

        Ok(IngestResult {
            name: name.to_string(),
            object_id,
            data_cid_hex: hex(&preview),
            width: 0,
            height: 0,
            kind: ObjectKind::Opaque,
            total_shards: 0,
            put_ms: staged_ms,
            outcome: IngestOutcome::Encoding,
        })
    }

    /// Background worker for [`Self::ingest_bytes_async`]. Runs the
    /// real encode + fanout, then flips the manifest to `Ready` (on
    /// success) or `Failed` (on error) and persists the catalog.
    /// Cache invalidation is unconditional — the placeholder that
    /// went in during staging is a decode-failing tombstone that
    /// must not be served after we finish.
    async fn run_encode_worker(self: Arc<Self>, name: String, body: Vec<u8>) {
        use std::sync::atomic::Ordering;

        // Acquire an ENCODE permit BEFORE the CPU-heavy RLNC work.
        // The HTTP handler released its MEDIUM permit the moment it
        // emitted 202, so without a dedicated throttle here the
        // spawned encoders would accumulate on the tokio scheduler
        // (490+ concurrent seen in the July 2026 soak → scheduler
        // starvation, /api/stats → 504). Reusing MEDIUM instead
        // ganged encoders + inbound HTTP on the same semaphore and
        // blocked put_new / mkdir / rmdir at 97 % 503. A dedicated
        // `encode_permits` (default 8 ≈ physical cores) keeps
        // encoder parallelism independent from HTTP MEDIUM: excess
        // async PUTs simply spend longer in `Encoding`, polling
        // clients see 503+Retry-After and back off — the natural
        // end-to-end throttle.
        let _permit = Arc::clone(&self.encode_permits)
            .acquire_owned()
            .await
            .expect("encode permit semaphore closed");

        let live = self.effective_live().await;
        // Result carries either the finished manifest (Ok) or a
        // best-effort partial manifest (Err) whose `object_id` /
        // `nodes` reflect what actually landed on the cluster. B11:
        // the cleanup path used to purge under the placeholder id
        // (zero), which never matched the shards fanned out under
        // the real data_cid-derived id — they stayed as orphans
        // until /api/gc.
        let result: Result<(Manifest, u32), (String, Option<Manifest>)> = if live.is_empty() {
            Err((
                "cluster has no live nodes at worker time".to_string(),
                None,
            ))
        } else {
            self.put_any(&name, &body, &live)
                .await
                .map(|(m, _, total)| (m, total))
        };

        let mut cat = self.catalog.write().await;
        match result {
            Ok((mut real, _total)) => {
                real.created_at_unix = now_unix();
                real.state = ManifestState::Ready;
                cat.insert(name.clone(), real);
                drop(cat);
                self.invalidate_cache(&name).await;
                if let Err(e) = self.persist_catalog().await {
                    eprintln!(
                        "PUT-async {name}: encode ok but catalog persist failed: {e:?}"
                    );
                    self.encode_failed_total.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.encode_completed_total.fetch_add(1, Ordering::Relaxed);
                    self.embed_object_in_background(name.clone());
                }
            }
            Err((err, partial)) => {
                eprintln!("PUT-async {name}: encode failed: {err}");
                if let Some(m) = cat.get_mut(&name) {
                    if m.state == ManifestState::Encoding {
                        m.state = ManifestState::Failed;
                    }
                }
                drop(cat);
                self.invalidate_cache(&name).await;
                let _ = self.persist_catalog().await;
                self.encode_failed_total.fetch_add(1, Ordering::Relaxed);

                // Best-effort shard cleanup: fire `Purge { object_id }`
                // at every live node using the REAL manifest surfaced
                // by `put_any` (has the data_cid-derived object_id).
                // Falling back to a fresh live-set: put_any's fanout
                // may have failed because the original `live` moved
                // under us, so re-fetch before the purge.
                if let Some(m) = partial {
                    let live = self.effective_live().await;
                    if !live.is_empty() {
                        if let Err(e) = holofs_client::purge_object(&m, &live).await {
                            eprintln!(
                                "PUT-async {name}: best-effort purge failed (will settle at /api/gc): {e:?}"
                            );
                        }
                    }
                }
            }
        }
        self.objects_encoding.fetch_sub(1, Ordering::Relaxed);
    }
}
