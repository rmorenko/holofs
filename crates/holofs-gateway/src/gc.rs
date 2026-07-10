//! orphan-shard garbage collection.
//!
//! The `gc_orphaned_shards` pass:
//!   1. Snapshots every shard hash referenced by the catalog +
//!      version archives (the *live* set).
//!   2. Asks every live node for its held-shards list.
//!   3. `PurgeByHash`'s the residue (held − live) node-by-node.
//!
//! `purge_orphans_of` helper (called by DELETE and PUT-replace)
//! stays in the monolith because both catalog-mutating paths
//! consume it too.

use std::time::Instant;

use holofs_client::{list_node_hashes, node_current_epoch, purge_node_by_hash_up_to};
use holofs_model::manifest::{Manifest, ObjectKind};

use crate::error::GatewayError;
use crate::Gateway;

/// per-node breakdown of one GC pass.
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
    /// embedding records kept after rewriting
    /// embeddings.bin. `None` when the embed feature is off.
    pub embeddings_kept: Option<u64>,
    /// embedding records dropped (orphan data_cid +
    /// tombstones). `None` when the embed feature is off.
    pub embeddings_dropped: Option<u64>,
    /// Wall-clock duration in ms.
    pub duration_ms: u128,
}

impl Gateway {
    /// garbage-collect orphan shards from every live
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
    /// **Concurrency (epoch-GC):** the pass no longer takes an
    /// exclusive `gc_barrier.write()` guard against writers. Shard
    /// safety comes from the epoch tag: every `Store::put` records a
    /// wall-clock write-epoch, and the pass gates each per-node
    /// purge with the pre-snapshot epoch it took at the very top of
    /// the run. Any shard whose stored epoch is `>` the snapshot
    /// was written *after* the GC pass started, so the node refuses
    /// to purge it even if it's in the "orphan" target set (the
    /// catalog snapshot froze before the write and so doesn't
    /// mention that hash).
    ///
    /// PUTs, restore_version, delete, scrub_tick now all run
    /// concurrently with GC. The one remaining serialisation point
    /// is the embed.bin rewrite at the tail of this pass — that one
    /// still takes the `gc_barrier` write guard to coordinate with
    /// `embed_object` (search.rs) whose append also holds the read
    /// guard.
    ///
    /// Returns a [`GcReport`] with per-node breakdown.
    pub async fn gc_orphaned_shards(&self) -> Result<GcReport, GatewayError> {
        use std::collections::HashSet;
        let t0 = Instant::now();
        // Snapshot the pass's cutoff epoch FIRST. Every subsequent
        // read (catalog, node held lists) may race concurrent PUTs;
        // those PUTs land with epoch > snapshot and are protected
        // by the node-side `PurgeByHashUpTo` gate below.
        let snapshot_epoch = holofs_core::time::now_unix_ms();

        // 1. Snapshot the live catalog hashes.
        //
        // Snapshot Arc<Manifest> for every decodable entry under a
        // short read-lock, then release the lock before the O(shards)
        // hash-set fill. Under a busy write workload the previous
        // "hold read-lock through the whole walk" pattern parked
        // every PUT/mkdir for the duration of GC (bottleneck #7 in
        // the review's §3a).
        //
        // alongside the shard-hash set we also build the
        // set of live `data_cid`s — used at the end of the pass to
        // tombstone embeddings whose owning object no longer exists
        // anywhere (catalog + version archives).
        let mut live: HashSet<[u8; 32]> = HashSet::new();
        let mut live_cids: HashSet<[u8; 32]> = HashSet::new();
        let mut manifests_scanned: u64 = 0;
        let catalog_snapshot: Vec<std::sync::Arc<Manifest>> = {
            let cat = self.catalog.read().await;
            cat.entries
                .iter()
                .filter(|(_, m)| m.kind != ObjectKind::Directory)
                .map(|(_, m)| std::sync::Arc::clone(m))
                .collect()
        };
        for m in &catalog_snapshot {
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
            // epoch-GC: pick the tighter of {gateway snapshot,
            // node-reported current epoch}. Using the node's own
            // clock as an upper bound sidesteps clock skew: if the
            // node's wall clock lags the gateway's, the gateway's
            // snapshot could otherwise flag a legitimate concurrent
            // PUT (stamped with the lower node clock) as purgeable.
            let node_epoch_now = node_current_epoch(&addr).await.unwrap_or(u64::MAX);
            let cutoff = snapshot_epoch.min(node_epoch_now);
            let held = match list_node_hashes(&addr).await {
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
                match purge_node_by_hash_up_to(&addr, orphans, cutoff).await {
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

        // embedding GC — rewrite embeddings.bin keeping
        // only records whose data_cid is still in `live_cids`. Also
        // strips tombstones for free (rewrite_keep drops empty-vec
        // records unconditionally). Bumps ann_generation so the next
        // semantic_search rebuilds the in-memory ANN index without
        // stale hits.
        //
        // epoch-GC: this is the ONLY thing still under the
        // `gc_barrier` write guard — the rewrite walks the file
        // whole-hog, so a concurrent `search::embed_object` append
        // would race it. The shard-GC block above no longer needs
        // the guard (see method-level doc).
        let _emb_guard = self.gc_barrier.write().await;
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

