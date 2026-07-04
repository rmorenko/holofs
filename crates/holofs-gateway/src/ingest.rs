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

use std::time::Instant;

use holofs_client::{put_object, put_object_replicated_blocks, LiveNodes};
use holofs_codec::image_io::load_photo_from_bytes;
use holofs_core::hash::hex;
use holofs_core::transform::coeff_layer;
use holofs_core::{K, LEVELS, NLAYERS, RED};
use holofs_model::manifest::{Manifest, ObjectEncoding, ObjectKind};
use holofs_model::path as catalog_path;

use crate::error::GatewayError;
use crate::util::{guess_opaque_content_type, guess_text_content_type, now_unix};
use crate::Gateway;

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
    /// Ingestion wall-clock time in milliseconds.
    pub put_ms: u128,
}

impl Gateway {
    /// Universal PUT: tries image → audio → text → opaque. Returns the
    /// finished Manifest (with shards already distributed), the kind
    /// label, and the total shard count.
    pub(crate) async fn put_any(
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
            kind: ObjectKind::Opaque,
            content_type,
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
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
        let (mut manifest, _kind_str, total_shards) = self
            .put_any(name, body, &live)
            .await
            .map_err(GatewayError::BadRequest)?;
        // stamp the manifest with creation time so the
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
        let prev = self.catalog.lock().await.get(name).cloned();
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
            .lock()
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
        })
    }
}
