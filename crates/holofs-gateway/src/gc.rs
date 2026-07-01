//! Stage 14.0 — orphan-shard garbage collection.
//!
//! The `gc_orphaned_shards` pass:
//!   1. Snapshots every shard hash referenced by the catalog +
//!      version archives (the *live* set).
//!   2. Asks every live node for its held-shards list.
//!   3. `PurgeByHash`'s the residue (held − live) node-by-node.
//!
//! Moved out of `http_gateway.rs` in Phase R1b.4. The
//! `purge_orphans_of` helper (called by DELETE and PUT-replace)
//! stays in the monolith because both catalog-mutating paths
//! consume it too.

use std::time::Instant;

use holofs_client::{list_node_hashes, purge_node_by_hash};
use holofs_model::manifest::{Manifest, ObjectKind};

use crate::error::GatewayError;
use crate::Gateway;

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
    /// **Concurrency (Stage 14.4):** the pass takes an exclusive
    /// `gc_barrier.write()` guard, so it waits for every in-flight
    /// PUT / restore / embed-append AND blocks new ones until it's
    /// done. Trade-off: PUTs are queued behind GC for the duration
    /// of one pass (≈40 ms on the dev catalog). That's fine for a
    /// manually-triggered `/api/gc`; a scheduled GC would want a
    /// smarter epoch-based scheme instead.
    ///
    /// Returns a [`GcReport`] with per-node breakdown.
    pub async fn gc_orphaned_shards(&self) -> Result<GcReport, GatewayError> {
        use std::collections::HashSet;
        // Stage 14.4: serialise against catalog-mutating writers
        // (PUTs, restore_version, embed_object). Waits for every
        // in-flight writer; blocks new ones until we're done.
        // Without this exclusive guard a fresh PUT during the GC
        // pass could land shards on a node *after* we snapshotted
        // its held list AND *before* we snapshotted the catalog
        // for the live set — the next node's held list would then
        // include `h_new` while our `live` set wouldn't, and the
        // subsequent PurgeByHash would silently delete the fresh
        // shard.
        let _gc_guard = self.gc_barrier.write().await;
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

