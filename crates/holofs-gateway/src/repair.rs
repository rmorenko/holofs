//! Read-side auto-repair + orphan-shard purging.
//!
//! - [`Gateway::decode_with_autorepair`] wraps
//!   [`holofs_client::get_object_up_to_layer`] with an eager
//!   [`ClientError::LayerLost`] fallback: repair the affected
//!   nodes in-place and retry once.
//! - [`Gateway::repair_object_inplace`] surgically calls
//!   [`repair_node`] only on nodes whose hash inventory is missing
//!   what `manifest.place_shard(...)` says should live there.
//!   Persists the mutated manifest back to disk.
//! - [`Gateway::purge_orphans_of`] finds shards uniquely referenced
//!   by one manifest (subtracting hashes still referenced by other
//!   catalog entries and version archives) and PurgeByHash's them
//!   node-by-node. Called by DELETE and PUT-replace so shared
//!   `data_cid` neighbours stay decodeable.
//!
//! Moved out of `http_gateway.rs` in Phase R1b.13.

use holofs_client::{
    get_object_blocks, get_object_up_to_layer, repair_node, repair_node_replicated, ClientError,
    LiveNodes,
};
use holofs_model::manifest::{Manifest, ObjectEncoding, ObjectKind};
use holofs_model::placement::{place_replicas, ShardKey};

use crate::error::GatewayError;
use crate::Gateway;

impl Gateway {
    /// Wrap [`get_object_up_to_layer`] with eager auto-repair on
    /// [`ClientError::LayerLost`]. The first decode attempt runs
    /// normally; on a LayerLost error we walk every live node,
    /// regenerate its missing shards via [`repair_node`], persist
    /// the mutated manifest back to the catalog (so the new
    /// `shard_hashes` survive a restart), and retry the decode
    /// once. The second LayerLost is permanent — we surface it.
    ///
    /// Why this exists: the old audit-reputation bug
    /// (Stage 14.x) silently emptied half the catalog's images
    /// overnight; even after we stopped the cascade, the only
    /// path back was a manual `curl -X PUT` per affected file.
    /// Auto-repair-on-read heals those holes inline whenever a
    /// user actually GETs an affected object, without requiring
    /// the operator to keep the original bytes lying around.
    ///
    /// Cost: a single failing GET pays one full
    /// `repair_node`-per-node pass — bounded at K shard-encode
    /// RPCs per live node. For the dev cluster (40 nodes × 444
    /// shards) that's ~1–2 s of latency on the first read; the
    /// repaired shards stay put for subsequent reads.
    pub(crate) async fn decode_with_autorepair(
        &self,
        name: &str,
        max_layer: u8,
    ) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
        use holofs_model::manifest::ObjectEncoding;
        let live = self.effective_live().await;
        let manifest = {
            let cat = self.catalog.lock().await;
            cat.get(name).cloned().ok_or_else(|| {
                ClientError::RemoteError(format!("decode_with_autorepair: {name} not in catalog"))
            })?
        };
        // Fork on encoding: RLNC uses gather+decode with an
        // auto-repair retry on LayerLost. Replicated uses the
        // block-fetch decoder — auto-repair for it lives at
        // shard-drop granularity (a block whose R replicas are
        // all dead is unrecoverable in this pass; a scheduled
        // repair heals it before the next GET).
        if let ObjectEncoding::Replicated { .. } = manifest.encoding {
            // Ask for every block up to `max_layer`; layers past
            // that stay zero, matching get_object_up_to_layer's
            // progressive-decode contract.
            let all_ids: Vec<Vec<u32>> = manifest
                .n_per_layer
                .iter()
                .enumerate()
                .map(|(l, &n)| {
                    if l as u8 > max_layer {
                        Vec::new()
                    } else {
                        (0..n).collect()
                    }
                })
                .collect();
            match get_object_blocks(&manifest, &live, &all_ids).await {
                Ok(v) => return Ok(v),
                Err(ClientError::LayerLost { channel, layer }) => {
                    self.auto_repairs_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Err(e) = self.repair_object_inplace(name).await {
                        self.auto_repair_failures_total
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        eprintln!("auto-repair: {name}: {e}");
                        return Err(ClientError::LayerLost { channel, layer });
                    }
                    let repaired = {
                        let cat = self.catalog.lock().await;
                        cat.get(name).cloned().ok_or_else(|| {
                            ClientError::RemoteError(format!(
                                "decode_with_autorepair: {name} disappeared mid-repair"
                            ))
                        })?
                    };
                    return match get_object_blocks(&repaired, &live, &all_ids).await {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            self.auto_repair_failures_total
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            Err(e)
                        }
                    };
                }
                Err(e) => return Err(e),
            }
        }
        match get_object_up_to_layer(&self.gf, &manifest, &live, max_layer).await {
            Ok(v) => Ok(v),
            Err(ClientError::LayerLost { channel, layer }) => {
                self.auto_repairs_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "auto-repair: {name} decode failed on (c={channel}, l={layer}); \
                     running repair_node across {} live nodes",
                    live.len()
                );
                if let Err(e) = self.repair_object_inplace(name).await {
                    self.auto_repair_failures_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("auto-repair: {name}: repair pass itself failed: {e}");
                    return Err(ClientError::LayerLost { channel, layer });
                }
                // Re-snapshot the (now-mutated) manifest from the
                // catalog and retry. `repair_object_inplace`
                // wrote it back, so `cat.get(name)` returns the
                // repaired version.
                let repaired = {
                    let cat = self.catalog.lock().await;
                    cat.get(name).cloned().ok_or_else(|| {
                        ClientError::RemoteError(format!(
                            "decode_with_autorepair: {name} disappeared from catalog mid-repair"
                        ))
                    })?
                };
                match get_object_up_to_layer(&self.gf, &repaired, &live, max_layer).await {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        self.auto_repair_failures_total
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Err(e)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Surgical auto-repair: find live nodes whose
    /// `list_node_hashes()` is MISSING any hash that
    /// `manifest.place_shard(...)` says should land on them, and
    /// call `repair_node` only on those. Skips healthy nodes so
    /// we don't churn shards out of well-placed buckets.
    /// Persists the mutated manifest back to the catalog so the
    /// new `shard_hashes` survive a restart.
    ///
    /// Iterative `repair_node` across all 40 nodes was an
    /// expensive footgun: each call purges its target's bucket
    /// before re-encoding, and the donor-set shrinks as nearby
    /// nodes get churned. A single-pass "only repair what's
    /// broken" loop converges in one shot.
    pub async fn repair_object_inplace(&self, name: &str) -> Result<(), ClientError> {
        use std::collections::HashSet;
        let live = self.effective_live().await;
        let mut manifest = {
            let cat = self.catalog.lock().await;
            cat.get(name).cloned().ok_or_else(|| {
                ClientError::RemoteError(format!("repair_object_inplace: {name} not in catalog"))
            })?
        };
        let mut rng = holofs_core::rng::Rng::new(u64::from_be_bytes(
            manifest.data_cid[0..8].try_into().unwrap(),
        ));
        let d = manifest.k as usize;

        // For each live node, compute the set of hashes that
        // *should* live on it. The placement math differs by
        // encoding (RLNC = 1 node per shard; Replicated = R
        // nodes per block), so fork on `manifest.encoding` up
        // front.
        for &node in live.iter() {
            let mut expected_on_node: HashSet<[u8; 32]> = HashSet::new();
            match manifest.encoding {
                ObjectEncoding::Rlnc => {
                    for c in 0..manifest.channels {
                        for l in 0..manifest.nlayers {
                            let n = manifest.n_per_layer[l as usize];
                            for idx in 0..n {
                                if manifest.place_shard(c, l, idx, &live) == Ok(node) {
                                    if let Some(h) = manifest
                                        .shard_hashes
                                        .get(c as usize)
                                        .and_then(|chan| chan.get(l as usize))
                                        .and_then(|per_l| per_l.get(idx as usize))
                                    {
                                        expected_on_node.insert(*h);
                                    }
                                }
                            }
                        }
                    }
                }
                ObjectEncoding::Replicated { replication, .. } => {
                    for c in 0..manifest.channels {
                        for l in 0..manifest.nlayers {
                            let n = manifest.n_per_layer[l as usize];
                            for idx in 0..n {
                                let key = ShardKey {
                                    object_id: manifest.object_id,
                                    channel: c,
                                    layer: l,
                                    shard_idx: idx,
                                };
                                let Ok(replicas) = place_replicas(key, replication, &live) else {
                                    continue;
                                };
                                if !replicas.contains(&node) {
                                    continue;
                                }
                                if let Some(h) = manifest
                                    .shard_hashes
                                    .get(c as usize)
                                    .and_then(|chan| chan.get(l as usize))
                                    .and_then(|per_l| per_l.get(idx as usize))
                                {
                                    expected_on_node.insert(*h);
                                }
                            }
                        }
                    }
                }
            }
            if expected_on_node.is_empty() {
                continue;
            }
            // Ask the node what it actually has.
            let held = match holofs_client::list_node_hashes(&manifest.nodes[node]).await {
                Ok(hs) => hs.into_iter().collect::<HashSet<[u8; 32]>>(),
                Err(e) => {
                    eprintln!(
                        "repair_object_inplace: list_node_hashes node {node} failed: {e}"
                    );
                    continue;
                }
            };
            // If the node already holds every expected hash,
            // leave it alone — running the per-node repair would
            // purge its bucket needlessly.
            if expected_on_node.is_subset(&held) {
                continue;
            }
            eprintln!(
                "auto-repair: {name} node {node}: {} of {} expected hashes missing, repairing",
                expected_on_node.difference(&held).count(),
                expected_on_node.len()
            );
            let repair_res = match manifest.encoding {
                ObjectEncoding::Rlnc => {
                    repair_node(&self.gf, &mut rng, &mut manifest, &live, node, d).await
                }
                ObjectEncoding::Replicated { .. } => {
                    repair_node_replicated(&mut manifest, &live, node)
                        .await
                        .map(|s| {
                            // Repurpose stats into RLNC-shaped
                            // RepairStats so callers reading
                            // metrics see uniform fields.
                            s
                        })
                }
            };
            if let Err(e) = repair_res {
                eprintln!("repair_object_inplace: {name} node {node}: {e}");
            }
        }
        let mut cat = self.catalog.lock().await;
        cat.insert(name.to_string(), manifest);
        drop(cat);
        // N4: best-effort persist — the shard-side repair is complete
        // and the in-memory manifest reflects it. If disk save fails
        // (`catalog_persist_failures_total` + ERROR log fire inside
        // `persist_catalog`) we still return Ok so the caller
        // (scrub_tick or decode_with_autorepair retry path) reports
        // success. The updated manifest is lost on the next restart,
        // but the same repair will run again — the failure surfaces
        // as a metric alert instead of a stuck GET path.
        if let Err(e) = self.persist_catalog().await {
            tracing::warn!(name = %name, error = %e, "repair persist failed (best-effort)");
        }
        Ok(())
    }

    /// Purge only the shards `manifest` references that are not also
    /// referenced by any other live catalog entry or version-archive
    /// manifest. Used by DELETE and (when versions are off) by
    /// PUT-replace.
    ///
    /// Why this exists: the node-side `Request::Purge { object_id }`
    /// deletes the entire `(object_id, channel, layer)` bucket on
    /// the node. Holofs derives `object_id` from `data_cid`, so two
    /// objects with byte-identical content share an `object_id` and
    /// share the same buckets after PUT-time dedup. Calling Purge
    /// on one such object yanks the shards out from under the other,
    /// breaking GETs and producing real (not transient) `margin=-K`
    /// warnings in the monitor — observed on `ocean.png` after a
    /// scratch `race-test.png` containing the same bytes was DELETEd
    /// during the §25 concurrency scenario.
    ///
    /// The fix walks the rest of the catalog + version archives,
    /// builds the set of hashes still referenced after `manifest`
    /// is conceptually removed (callers must remove from catalog
    /// FIRST), subtracts that from `manifest.shard_hashes`, and
    /// asks every live node to `PurgeByHash` the residue.
    pub(crate) async fn purge_orphans_of(
        &self,
        manifest: &Manifest,
        live_nodes: &LiveNodes,
        exclude_name: Option<&str>,
    ) -> Result<(), GatewayError> {
        use std::collections::HashSet;
        // 1. Hashes the deleted manifest claimed to own.
        let mut owned: HashSet<[u8; 32]> = HashSet::new();
        for chan in &manifest.shard_hashes {
            for per_l in chan {
                for h in per_l {
                    owned.insert(*h);
                }
            }
        }
        if owned.is_empty() {
            return Ok(());
        }
        // 2. Hashes still referenced by the rest of the catalog. The
        //    PUT-replace caller hasn't yet removed `old` from the
        //    catalog under `name`, so we skip that name explicitly —
        //    otherwise `owned` would always empty itself out.
        {
            let cat = self.catalog.lock().await;
            for (n, m) in cat.entries.iter() {
                if m.kind == ObjectKind::Directory {
                    continue;
                }
                if Some(n.as_str()) == exclude_name {
                    continue;
                }
                for chan in &m.shard_hashes {
                    for per_l in chan {
                        for h in per_l {
                            owned.remove(h);
                            if owned.is_empty() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        // 3. Hashes still referenced by version archives on disk.
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
                                for chan in &m.shard_hashes {
                                    for per_l in chan {
                                        for h in per_l {
                                            owned.remove(h);
                                            if owned.is_empty() {
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // 4. Whatever is left in `owned` is unique to this manifest
        //    and safe to purge. Send a PurgeByHash to every live node.
        let orphans: Vec<[u8; 32]> = owned.into_iter().collect();
        for &node_idx in live_nodes {
            let addr = self.cluster.node_addrs[node_idx].clone();
            if let Err(e) = holofs_client::purge_node_by_hash(&addr, orphans.clone()).await {
                eprintln!("purge_orphans_of: PurgeByHash failed on {addr}: {e}");
            }
        }
        Ok(())
    }
}
