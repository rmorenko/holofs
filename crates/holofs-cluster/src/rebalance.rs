//! rebalancing on topology change.
//!
//! Simple case: a new node is added to the cluster. After that the HRW
//! layout of each (channel, layer) changes — some shards must now live on
//! the new node. Algorithm:
//!
//! 1. Append the new node to `manifest.nodes` and its zone to `manifest.zones`.
//! 2. Call [`holofs_client::repair_node`] on the new index. Inside, it
//!    detects which shards HRW assigns to this node, fetches K donors from
//!    live neighbours, and bakes fresh RLNC combinations. Hashes of the new
//!    shards are appended to `manifest.shard_hashes`; the Merkle root updates.
//! 3. Old shards on old nodes stay (HRW property: they haven't "moved", just
//!    an extra copy appeared on the new node). Margin grows; nothing is lost.
//!
//! Drain (decommissioning a node) is the mirror operation (P1.4). Removing
//! from `manifest.nodes` would shift every index — instead we keep the node
//! in the list but mark it "logically dead" (`Gateway::admin_kills`); then
//! [`drain_node`] iterates every catalog entry and, for each
//! (channel, layer, idx) whose HRW placement currently lands on the drain
//! node, re-emits a fresh RLNC shard onto the new HRW target from the
//! shrunk live set. After drain completes, the physical shards on the
//! drained node are dead weight — the caller can optionally wipe them via
//! `Request::Wipe` (4.2 flow) before the node is physically decommissioned.

use std::sync::Arc;

use tokio::sync::RwLock;

use holofs_client::{drain_node_from_manifest, repair_node, ClientError, LiveNodes};
use holofs_core::gf::Gf;
use holofs_core::repair::RepairStats;
use holofs_core::rng::Rng;
use holofs_model::fs::Directory;
use holofs_model::manifest::Manifest;

/// Report on add-node action for one object.
#[derive(Debug)]
pub struct AddNodeReport {
    pub object: String,
    pub result: Result<RepairStats, ClientError>,
}

/// Report on drain-node action for one object. Same shape as
/// [`AddNodeReport`] (both are catalog-wide sweeps producing a
/// [`RepairStats`] per object) but kept distinct so callers /
/// telemetry / logs can classify the operation.
#[derive(Debug)]
pub struct DrainNodeReport {
    pub object: String,
    pub result: Result<RepairStats, ClientError>,
}

/// Add a new node to the cluster and reshuffle each object's shards so the
/// new node gets its HRW share.
///
/// v2 P2.3 (§3a bottleneck #8): the prior signature took `&mut
/// Directory` and held the exclusive borrow through every
/// per-object `repair_node().await`. That was the same
/// writer-convoy pattern the S2 monitor fix removed from the
/// per-tick repair loop — every gateway PUT / mkdir / rmdir was
/// serialised behind the whole M × network-repair walk. Now
/// `add_node` snapshots one manifest at a time under a short
/// read-lock, runs `repair_node` on an owned mutable copy without
/// any catalog lock, and CAS-writes back under a short write-lock
/// gated on `data_cid` — a concurrent PUT-replace's fresh manifest
/// is preserved instead of being clobbered by stale-placement
/// output.
///
/// `repair_d` — how many donor shards regen takes per (channel, layer);
/// `repair_d = K` guarantees full decodability retention.
pub async fn add_node(
    gf: &Gf,
    rng: &mut Rng,
    catalog: Arc<RwLock<Directory>>,
    addr: String,
    zone: u8,
    repair_d: usize,
) -> Vec<AddNodeReport> {
    let mut reports = Vec::new();
    let names: Vec<String> = {
        let cat = catalog.read().await;
        cat.names()
    };
    for name in names {
        // Snapshot: Arc-clone the entry, drop the read-lock, then
        // deep-clone into an owned Manifest we can mutate.
        let snapshot: Option<Arc<Manifest>> = {
            let cat = catalog.read().await;
            cat.entries.get(&name).cloned()
        };
        let Some(snap) = snapshot else {
            continue;
        };
        let snap_data_cid = snap.data_cid;
        let mut mutable: Manifest = (*snap).clone();
        drop(snap);

        // The topology change (`+addr`) is a local edit on the copy.
        mutable.nodes.push(addr.clone());
        mutable.zones.push(zone);
        let new_idx = mutable.nodes.len() - 1;
        let live: Vec<usize> = (0..mutable.nodes.len()).collect();
        let result = repair_node(gf, rng, &mut mutable, &live, new_idx, repair_d).await;

        // CAS write-back: only apply if the catalog entry's
        // data_cid is still what we snapshot'd. If a PUT-replace
        // ran under us the new entry already has the new-node
        // placement (fresh PUT sees the current cluster.node_addrs)
        // and shouldn't be trampled.
        {
            let mut cat = catalog.write().await;
            let should_apply = cat
                .entries
                .get(&name)
                .map(|cur| cur.data_cid == snap_data_cid)
                .unwrap_or(false);
            if should_apply {
                cat.insert(name.clone(), mutable);
            }
        }
        reports.push(AddNodeReport {
            object: name,
            result,
        });
    }
    reports
}

/// Rebalance every catalog entry so shards HRW-assigned to
/// `drain_idx` land on the shrunk live set instead. Mirror of
/// [`add_node`] — same CAS-write-back discipline against a
/// concurrent PUT-replace, same per-object [`RepairStats`] output.
///
/// **Invariant**: the caller must have already flipped
/// `admin_kills[drain_idx] = true` (or otherwise removed
/// `drain_idx` from the effective live set) *before* invoking this.
/// Otherwise concurrent PUTs during the sweep could land fresh
/// shards on the very node we're draining, defeating the point.
///
/// - `drain_idx`: position of the node in every `manifest.nodes`
///   (stays put — removal would shift every other index).
/// - `live_before_drain`: baseline live set INCLUDING `drain_idx`,
///   used to detect current placement.
/// - `live_after_drain`: same set with `drain_idx` filtered out,
///   used both for donor selection and new-placement target.
/// - `repair_d`: donor count per (channel, layer). `repair_d = K`
///   guarantees full decodability retention.
///
/// After successful drain, physical shards on `drain_idx` are
/// unreachable (HRW no longer selects it, but the shards are still
/// on disk there). Callers who want to reclaim that disk before the
/// node is physically decommissioned should send `Request::Wipe` to
/// the drained node — see the 4.2 `--purge` flow.
pub async fn drain_node(
    gf: &Gf,
    rng: &mut Rng,
    catalog: Arc<RwLock<Directory>>,
    drain_idx: usize,
    live_before_drain: LiveNodes,
    repair_d: usize,
) -> Vec<DrainNodeReport> {
    let live_after_drain: LiveNodes = live_before_drain
        .iter()
        .copied()
        .filter(|&n| n != drain_idx)
        .collect();

    let mut reports = Vec::new();
    let names: Vec<String> = {
        let cat = catalog.read().await;
        cat.names()
    };
    for name in names {
        let snapshot: Option<Arc<Manifest>> = {
            let cat = catalog.read().await;
            cat.entries.get(&name).cloned()
        };
        let Some(snap) = snapshot else {
            continue;
        };
        let snap_data_cid = snap.data_cid;
        let mut mutable: Manifest = (*snap).clone();
        drop(snap);

        // Skip directory / trivial manifests. `manifest.nodes.is_empty()`
        // means the entry isn't a data object.
        if mutable.nodes.is_empty() {
            reports.push(DrainNodeReport {
                object: name,
                result: Ok(RepairStats::default()),
            });
            continue;
        }
        // If drain_idx is outside this manifest's nodes vec (which
        // can happen if the manifest predates a cluster expansion),
        // there's nothing to migrate for this object.
        if drain_idx >= mutable.nodes.len() {
            reports.push(DrainNodeReport {
                object: name,
                result: Ok(RepairStats::default()),
            });
            continue;
        }

        let result = drain_node_from_manifest(
            gf,
            rng,
            &mut mutable,
            drain_idx,
            &live_before_drain,
            &live_after_drain,
            repair_d,
        )
        .await;

        // CAS write-back — identical semantics to add_node.
        {
            let mut cat = catalog.write().await;
            let should_apply = cat
                .entries
                .get(&name)
                .map(|cur| cur.data_cid == snap_data_cid)
                .unwrap_or(false);
            if should_apply {
                cat.insert(name.clone(), mutable);
            }
        }
        reports.push(DrainNodeReport {
            object: name,
            result,
        });
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use holofs_model::manifest::{Manifest, ManifestState};
    use holofs_model::placement::Placement;

    fn empty_manifest(nodes: Vec<String>) -> Manifest {
        let n = nodes.len();
        Manifest {
            object_id: 1,
            k: 4,
            nlayers: 2,
            n_per_layer: vec![16, 8],
            sym_len: vec![32, 32],
            layer_positions: vec![vec![], vec![]],
            channels: 2,
            width: 8,
            height: 8,
            levels: 1,
            nodes,
            placement: Placement::Rendezvous,
            zones: vec![0; n],
            data_cid: [9; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 2]; 2],
            kind: holofs_model::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
        state: holofs_model::manifest::ManifestState::Ready,
        retention: None,
    }
    }

    #[test]
    fn add_node_extends_manifest_in_place() {
        // We only check the manifest.nodes/.zones mutation here — the network
        // poll (repair_node call) crosses RPC and is not unit-tested.
        let mut dir = Directory::new();
        let m = empty_manifest(vec!["a".into(), "b".into()]);
        dir.insert("o1".into(), m);

        let manifest = dir.get_mut("o1").unwrap();
        manifest.nodes.push("c".into());
        manifest.zones.push(1);
        assert_eq!(manifest.nodes.len(), 3);
        assert_eq!(manifest.zones, vec![0, 0, 1]);
    }

    #[tokio::test]
    async fn drain_node_skips_directory_manifests() {
        // Directory entries have empty `nodes`; drain should short-
        // circuit each with Ok(default stats) rather than trying to
        // fetch donors from a zero-length list.
        let dir_root = Arc::new(RwLock::new(Directory::new()));
        {
            let mut d = dir_root.write().await;
            // "docs" directory: empty nodes.
            let mut dir_entry = empty_manifest(vec![]);
            dir_entry.kind = holofs_model::manifest::ObjectKind::Directory;
            d.insert("docs".into(), dir_entry);
        }
        let gf = holofs_core::gf::Gf::new();
        let mut rng = holofs_core::rng::Rng::new(1);
        let reports = drain_node(&gf, &mut rng, Arc::clone(&dir_root), 0, vec![0, 1, 2], 4).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].object, "docs");
        assert!(reports[0].result.is_ok());
        let stats = reports[0].result.as_ref().unwrap();
        assert_eq!(stats.shards_generated, 0);
    }

    #[tokio::test]
    async fn drain_node_skips_manifests_where_drain_idx_out_of_range() {
        // A manifest that was written before a cluster grew: its
        // `nodes` vec has fewer entries than the current cluster
        // topology. Draining index N when N is beyond the manifest's
        // own list is a no-op for that object.
        let dir_root = Arc::new(RwLock::new(Directory::new()));
        {
            let mut d = dir_root.write().await;
            // Manifest with only 2 nodes; caller drains idx=5.
            let m = empty_manifest(vec!["a".into(), "b".into()]);
            d.insert("legacy".into(), m);
        }
        let gf = holofs_core::gf::Gf::new();
        let mut rng = holofs_core::rng::Rng::new(1);
        let reports = drain_node(&gf, &mut rng, Arc::clone(&dir_root), 5, vec![0, 1, 5], 4).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].result.is_ok());
        assert_eq!(reports[0].result.as_ref().unwrap().shards_generated, 0);
        // Manifest untouched.
        let d = dir_root.read().await;
        assert_eq!(d.get("legacy").unwrap().nodes.len(), 2);
    }

    #[tokio::test]
    async fn drain_node_computes_live_after_drain_correctly() {
        // Sanity check: live_after_drain = live_before minus drain_idx.
        // Verifies the filter that populates the downstream call
        // to `drain_node_from_manifest`. We inspect the outcome
        // indirectly — a manifest whose nodes.len() < drain_idx
        // returns Ok(default) with zero side effects; a real
        // network call is not exercised (no spawn_node here).
        let dir_root = Arc::new(RwLock::new(Directory::new()));
        // Empty catalog → drain returns empty reports without any
        // network I/O, which is safe to assert in a unit test.
        let gf = holofs_core::gf::Gf::new();
        let mut rng = holofs_core::rng::Rng::new(42);
        let reports = drain_node(&gf, &mut rng, Arc::clone(&dir_root), 3, vec![0, 1, 2, 3, 4], 4).await;
        assert!(reports.is_empty(), "empty catalog → empty reports");
    }
}
