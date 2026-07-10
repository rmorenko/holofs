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
//! Drain (decommissioning a node) is more complex: removing from `nodes`
//! shifts every index. The simple solution is to keep "logically dead" nodes
//! in a separate list outside the manifest and let the health monitor
//! re-emit shards on live neighbours. Deferred (TODO).

use std::sync::Arc;

use tokio::sync::RwLock;

use holofs_client::{repair_node, ClientError};
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
}
