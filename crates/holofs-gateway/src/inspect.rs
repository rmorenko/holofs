//! `/inspect/<name>` view-model.
//!
//! `inspect` builds the per-(channel, layer) layout for the shard
//! grid page; `shard_payload` fetches (and verifies) one specific
//! shard for the zoom / detail view. Both are read-only —
//! everything runs against catalog snapshots + gather RPCs, no
//! catalog mutation.
//!

use std::sync::Arc;

use holofs_client::gather_layer;
use holofs_core::hash::hex;

use crate::error::GatewayError;
use crate::Gateway;


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


impl Gateway {

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
                    // Skip shards we can't place — happens when the
                    // cluster is fully down. The inspect view is a
                    // diagnostic; we'd rather render an incomplete
                    // grid than 500 the whole page.
                    let Ok(node_idx) = manifest.place_shard(c, l, idx, &live) else {
                        continue;
                    };
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
        let node_idx = manifest
            .place_shard(c, l, idx, &live)
            .map_err(|_| GatewayError::ClusterDegraded)?;
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

        // cache the per-layer gather. The inspect grid renders
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
}
