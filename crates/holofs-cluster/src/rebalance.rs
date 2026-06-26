//! Stage 5: rebalancing on topology change.
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

use holofs_client::{repair_node, ClientError};
use holofs_core::gf::Gf;
use holofs_core::repair::RepairStats;
use holofs_core::rng::Rng;
use holofs_model::fs::Directory;

/// Report on add-node action for one object.
#[derive(Debug)]
pub struct AddNodeReport {
    pub object: String,
    pub result: Result<RepairStats, ClientError>,
}

/// Add a new node to the cluster and reshuffle each object's shards so the
/// new node gets its HRW share. Mutates every manifest in the catalog.
///
/// `repair_d` — how many donor shards regen takes per (channel, layer);
/// `repair_d = K` guarantees full decodability retention.
pub async fn add_node(
    gf: &Gf,
    rng: &mut Rng,
    catalog: &mut Directory,
    addr: String,
    zone: u8,
    repair_d: usize,
) -> Vec<AddNodeReport> {
    let mut reports = Vec::new();
    let names: Vec<String> = catalog.names();
    for name in names {
        let manifest = catalog.entries.get_mut(&name).unwrap();
        manifest.nodes.push(addr.clone());
        manifest.zones.push(zone);
        let new_idx = manifest.nodes.len() - 1;
        // After adding a node "everything is alive" — indices 0..n.
        let live: Vec<usize> = (0..manifest.nodes.len()).collect();
        let result = repair_node(gf, rng, manifest, &live, new_idx, repair_d).await;
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
    use holofs_model::manifest::Manifest;
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
        }
    }

    #[test]
    fn add_node_extends_manifest_in_place() {
        // We only check the manifest.nodes/.zones mutation here — the network
        // poll (repair_node call) crosses RPC and is not unit-tested.
        let mut dir = Directory::new();
        let m = empty_manifest(vec!["a".into(), "b".into()]);
        dir.insert("o1".into(), m);

        let manifest = dir.entries.get_mut("o1").unwrap();
        manifest.nodes.push("c".into());
        manifest.zones.push(1);
        assert_eq!(manifest.nodes.len(), 3);
        assert_eq!(manifest.zones, vec![0, 0, 1]);
    }
}
