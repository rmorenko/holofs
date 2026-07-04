//! Cluster health, stats, and background scrub — read-side snapshots
//! plus the periodic self-heal that keeps the GET path from ever
//! surfacing shard loss to users.
//!
//! - [`Gateway::api_stats`] — one-shot cluster snapshot (`GET /api/stats`).
//! - [`Gateway::health_index_data`] — per-node table + object list
//!   for the `/health` landing page.
//! - [`Gateway::object_health`] — per-object margin + Monte Carlo
//!   loss simulation (`GET /health/<name>`).
//! - [`Gateway::toggle_admin_kill`] — flip the admin-disabled flag
//!   for a node from the UI.
//! - [`Gateway::scrub_tick`] — one background-scrub pass; walks the
//!   catalog, queries every live node's hash inventory once, and
//!   heals any object whose expected hashes are missing.
//!

use std::sync::Arc;

use holofs_model::manifest::ObjectKind;

use crate::error::GatewayError;
use crate::Gateway;

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
    /// Number of times a GET path hit a `ClientError::LayerLost` and
    /// kicked off an inline `repair_node` pass to heal the cluster.
    /// Incremented in `decode_with_autorepair` each time the first
    /// attempt fails. A non-zero value here means the cluster is
    /// silently fixing itself on the read path — useful for spotting
    /// upstream shard-loss (audit reputation cascade, GC race, etc.).
    pub auto_repairs_total: u64,
    /// Subset of [`Self::auto_repairs_total`] where the post-repair
    /// retry ALSO failed — i.e. the object is irrecoverable from the
    /// shards currently on disk. The GET ultimately surfaces a 5xx to
    /// the caller.
    pub auto_repair_failures_total: u64,
    /// Background-scrub passes completed. Bumped by [`Gateway::scrub_tick`]
    /// once per tick regardless of whether it found anything to repair.
    pub scrub_runs_total: u64,
    /// Objects the background scrub repaired *before* any user GET
    /// tripped on them. High values here mean the cluster is silently
    /// healing itself; pair with `auto_repairs_total` to see how much
    /// damage the user-visible path was catching before scrub picked
    /// it up.
    pub scrub_repairs_total: u64,
}

/// Result of one [`Gateway::scrub_tick`] pass.
#[derive(Debug, Clone, Default)]
pub struct ScrubReport {
    /// Non-directory objects walked this tick.
    pub objects_scanned: u64,
    /// Objects where at least one expected hash was missing on its
    /// canonical node and the repair succeeded.
    pub objects_repaired: u64,
    /// Objects where the repair pass itself failed (donor set short
    /// of K — fundamental data loss, not transient).
    pub objects_repair_failed: u64,
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

impl Gateway {
    /// One pass of the background scrub task: walk the catalog,
    /// query every live node's hash inventory once, and for any
    /// object whose `place_shard`-expected hashes aren't where they
    /// should be, run [`Self::repair_object_inplace`] so the GET
    /// path never sees the 503.
    ///
    /// Cost-per-tick: O(catalog × live nodes) `list_node_hashes`
    /// RPCs (fast: each node hands back its full hash table once),
    /// plus per-affected-object repair (bounded). Designed to run
    /// every ~10 min in the background — the long interval keeps the
    /// repair cost diffuse, while still catching damage well before
    /// a user notices.
    ///
    /// epoch-GC: no longer takes `gc_barrier`. Any shards this
    /// pass writes via `repair_object_inplace` get a fresh epoch
    /// from `Store::put`, so a concurrent full-GC pass can't purge
    /// them mid-flight.
    pub async fn scrub_tick(self: &Arc<Self>) -> ScrubReport {
        use std::collections::{HashMap, HashSet};
        self.scrub_runs_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Snapshot live + catalog. We only need names of decodable
        // (non-directory) objects.
        let live = self.effective_live().await;
        let names: Vec<String> = {
            let cat = self.catalog.lock().await;
            cat.entries
                .iter()
                .filter(|(_, m)| m.kind != ObjectKind::Directory && !m.nodes.is_empty())
                .map(|(n, _)| n.clone())
                .collect()
        };
        if names.is_empty() || live.is_empty() {
            return ScrubReport::default();
        }

        // Pull each live node's full hash inventory once. Reused
        // across every object check below — single-shot RPC per
        // node instead of per-(node, object).
        let mut held_by_node: HashMap<usize, HashSet<[u8; 32]>> = HashMap::new();
        for &node in live.iter() {
            let addr = self.cluster.node_addrs[node].clone();
            match holofs_client::list_node_hashes(&addr).await {
                Ok(hs) => {
                    held_by_node.insert(node, hs.into_iter().collect());
                }
                Err(_) => {
                    // Skip nodes that don't answer this tick;
                    // monitor will flag them via LivenessChange.
                }
            }
        }

        // Walk catalog and identify which objects need repair.
        let mut to_repair: Vec<String> = Vec::new();
        {
            let cat = self.catalog.lock().await;
            'outer: for name in &names {
                let Some(manifest) = cat.entries.get(name) else {
                    continue;
                };
                for c in 0..manifest.channels {
                    for l in 0..manifest.nlayers {
                        let n = manifest.n_per_layer[l as usize];
                        for idx in 0..n {
                            let Ok(node) = manifest.place_shard(c, l, idx, &live) else {
                                continue;
                            };
                            let hash = match manifest
                                .shard_hashes
                                .get(c as usize)
                                .and_then(|chan| chan.get(l as usize))
                                .and_then(|per_l| per_l.get(idx as usize))
                            {
                                Some(h) => *h,
                                None => continue,
                            };
                            // Skip nodes we couldn't query this
                            // tick — re-checking next tick is
                            // cheaper than guessing.
                            let Some(held) = held_by_node.get(&node) else {
                                continue;
                            };
                            if !held.contains(&hash) {
                                to_repair.push(name.clone());
                                continue 'outer;
                            }
                        }
                    }
                }
            }
        }

        // Repair the affected objects one at a time. `repair_object_inplace`
        // re-snapshots `live` inside and persists the mutated manifest.
        // Under epoch-GC any fresh shard it writes carries a
        // post-snapshot epoch, so a concurrent full-GC pass can't
        // purge it even if the orphan diff briefly thinks it should.
        let mut repaired_ok = 0u64;
        let mut repaired_failed = 0u64;
        for name in &to_repair {
            match self.repair_object_inplace(name).await {
                Ok(()) => {
                    repaired_ok += 1;
                    self.scrub_repairs_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    repaired_failed += 1;
                    eprintln!("scrub: {name}: repair failed: {e}");
                }
            }
        }
        ScrubReport {
            objects_scanned: names.len() as u64,
            objects_repaired: repaired_ok,
            objects_repair_failed: repaired_failed,
        }
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
                ObjectKind::Image => counts.image += 1,
                ObjectKind::Audio => counts.audio += 1,
                ObjectKind::Text => counts.text += 1,
                ObjectKind::Opaque => counts.opaque += 1,
                ObjectKind::Directory => counts.directory += 1,
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
            auto_repairs_total: self
                .auto_repairs_total
                .load(std::sync::atomic::Ordering::Relaxed),
            auto_repair_failures_total: self
                .auto_repair_failures_total
                .load(std::sync::atomic::Ordering::Relaxed),
            scrub_runs_total: self
                .scrub_runs_total
                .load(std::sync::atomic::Ordering::Relaxed),
            scrub_repairs_total: self
                .scrub_repairs_total
                .load(std::sync::atomic::Ordering::Relaxed),
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
}
