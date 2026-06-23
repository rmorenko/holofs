//! Integration test for the distributed layer:
//! 8 nodes on random localhost ports, PUT/GET/REPAIR of a small object.

use std::net::Ipv4Addr;

use holofs_client::{
    auth_check, discover_live, discover_live_with_whitelist, gather_layer, get_object, put_object,
    repair_node,
};
use holofs_cluster::audit::{audit_shard, AuditOutcome};
use holofs_cluster::rebalance::add_node;
use holofs_cluster::reputation::Reputation;
use holofs_core::gf::Gf;
use holofs_core::merkle::shard_hash;
use holofs_core::rlnc::{decode_layer, Shard};
use holofs_core::rng::Rng;
use holofs_core::transform::haar_forward;
use holofs_core::K;
use holofs_model::manifest::Manifest;
use holofs_model::placement::{place, Placement, ShardKey};
use holofs_storage::identity::NodeIdentity;
use holofs_storage::node_service::{spawn_node, spawn_node_full, SharedStore};
use holofs_storage::whitelist::{Whitelist, WhitelistEntry};

const W: usize = 32;
const H: usize = 32;
const LEVELS: usize = 2;
const NLAYERS: usize = LEVELS + 1;
const N_NODES: usize = 8;
const RED: [f32; NLAYERS] = [3.0, 2.5, 2.0];

fn small_coeff_layer(x: usize, y: usize) -> usize {
    let llw = W >> LEVELS;
    let llh = H >> LEVELS;
    if x < llw && y < llh {
        return 0;
    }
    for l in (1..=LEVELS).rev() {
        let bw = W >> (l - 1);
        let bh = H >> (l - 1);
        let iw = W >> l;
        let ih = H >> l;
        if x < bw && y < bh && !(x < iw && y < ih) {
            return LEVELS - l + 1;
        }
    }
    LEVELS
}

async fn spawn_cluster(n: usize) -> (Vec<String>, Vec<SharedStore>) {
    let mut addrs = Vec::with_capacity(n);
    let mut stores = Vec::with_capacity(n);
    for _ in 0..n {
        let (a, s, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn node");
        addrs.push(a.to_string());
        stores.push(s);
    }
    (addrs, stores)
}

fn build_manifest(nodes: Vec<String>) -> Manifest {
    let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
    for y in 0..H {
        for x in 0..W {
            layer_positions[small_coeff_layer(x, y)].push((y * W + x) as u32);
        }
    }
    let n_per_layer: Vec<u32> = (0..NLAYERS)
        .map(|l| (K as f32 * RED[l]).round() as u32)
        .collect();
    let sym_len: Vec<u32> = layer_positions
        .iter()
        .map(|pos| {
            let bytes = pos.len() * 4;
            ((bytes + K - 1) / K) as u32
        })
        .collect();
    let n_nodes = nodes.len();
    Manifest {
        object_id: 0, // filled by put_object
        k: K as u16,
        nlayers: NLAYERS as u8,
        n_per_layer,
        sym_len,
        layer_positions,
        channels: 3,
        width: W as u32,
        height: H as u32,
        levels: LEVELS as u8,
        nodes,
        placement: Placement::Rendezvous,
        zones: vec![0; n_nodes],
        data_cid: [0; 32],
        merkle_root: [0; 32],
        shard_hashes: vec![vec![Vec::new(); NLAYERS]; 3],
        kind: holofs_model::manifest::ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: 0,
    }
}

fn synth_channels() -> Vec<Vec<f32>> {
    let mut r = vec![0f32; W * H];
    let mut g = vec![0f32; W * H];
    let mut b = vec![0f32; W * H];
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32;
            let fy = y as f32;
            let i = y * W + x;
            r[i] = 120.0 + 50.0 * (fx * 0.3).sin();
            g[i] = 100.0 + 60.0 * (fy * 0.4).cos();
            b[i] = 80.0 + 70.0 * ((fx + fy) * 0.2).sin();
        }
    }
    vec![r, g, b]
}

fn psnr(a: &[Vec<f32>], b: &[Vec<f32>]) -> f64 {
    let mut mse = 0f64;
    let mut n = 0u64;
    for c in 0..a.len() {
        for i in 0..a[c].len() {
            let d = (a[c][i] - b[c][i]) as f64;
            mse += d * d;
            n += 1;
        }
    }
    mse /= n as f64;
    if mse < 1e-9 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

#[tokio::test]
async fn put_then_get_roundtrip_exact() {
    let gf = Gf::new();
    let (addrs, _stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();

    let live = discover_live(&manifest).await;
    assert_eq!(live.len(), N_NODES);

    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();
    assert_ne!(manifest.data_cid, [0; 32]);
    assert_ne!(manifest.merkle_root, [0; 32]);
    assert_ne!(manifest.object_id, 0);

    let recon = get_object(&gf, &manifest, &live).await.unwrap();
    let p = psnr(&channels, &recon);
    assert!(p > 80.0, "PSNR should be high, got {p:.1} dB");
}

#[tokio::test]
async fn placement_lands_shards_only_on_chosen_node() {
    let gf = Gf::new();
    let (addrs, stores) = spawn_cluster(4).await;
    let mut manifest = build_manifest(addrs);
    let live: Vec<usize> = (0..4).collect();
    let channels = synth_channels();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    let mut expected = vec![0usize; 4];
    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            for idx in 0..manifest.n_per_layer[l as usize] {
                let key = ShardKey {
                    object_id: manifest.object_id,
                    channel: c,
                    layer: l,
                    shard_idx: idx,
                };
                expected[place(manifest.placement, key, 4, &live)] += 1;
            }
        }
    }
    for (i, store) in stores.iter().enumerate() {
        let actual = store.lock().await.total();
        assert_eq!(
            actual, expected[i],
            "node {i}: expected {} shards, got {actual}",
            expected[i]
        );
    }
}

#[tokio::test]
async fn repair_recovers_after_progressive_kills() {
    let gf = Gf::new();
    let mut rng = Rng::new(0x1234);
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();
    let root_before = manifest.merkle_root;

    for victim in 0..4 {
        stores[victim].lock().await.wipe();
        let st = repair_node(&gf, &mut rng, &mut manifest, &live, victim, K)
            .await
            .unwrap();
        assert_eq!(st.layers_unrecoverable, 0);
    }
    // Merkle root must change: new hashes appeared after the repair.
    assert_ne!(manifest.merkle_root, root_before);

    let recon = get_object(&gf, &manifest, &live).await.unwrap();
    let p = psnr(&channels, &recon);
    assert!(p > 80.0, "PSNR dropped after a series of repairs: {p:.1} dB");
}

#[tokio::test]
async fn get_fails_gracefully_when_layer_unrecoverable() {
    let gf = Gf::new();
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    for s in &stores {
        s.lock().await.wipe();
    }
    let err = get_object(&gf, &manifest, &live).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("is not decodable"),
        "expected LayerLost, got: {msg}"
    );
}

#[tokio::test]
async fn gather_layer_pulls_across_all_live_nodes() {
    let gf = Gf::new();
    let (addrs, _stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            let shards = gather_layer(&manifest, &live, c, l).await.unwrap();
            assert_eq!(
                shards.len(),
                manifest.n_per_layer[l as usize] as usize,
                "layer (c={c}, l={l})"
            );
            let refs: Vec<&Shard> = shards.iter().collect();
            let sl = manifest.sym_len[l as usize] as usize;
            assert!(decode_layer(&gf, &refs, sl).is_some());
        }
    }

    let mut plane = channels[0].clone();
    haar_forward(&mut plane, W, H, LEVELS);
    let mut bytes = Vec::new();
    for &p in &manifest.layer_positions[0] {
        bytes.extend_from_slice(&plane[p as usize].to_le_bytes());
    }
    assert!(!bytes.is_empty());
}

// === Stage 3: integrity and addressing =====================================

#[tokio::test]
async fn put_is_deterministic_and_dedupes() {
    let gf = Gf::new();
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest_a = build_manifest(addrs.clone());
    let mut manifest_b = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();

    put_object(&gf, &mut manifest_a, &live, &channels)
        .await
        .unwrap();
    let mut total_after_first = 0usize;
    for s in &stores {
        total_after_first += s.lock().await.total();
    }

    // Second PUT of the same content.
    put_object(&gf, &mut manifest_b, &live, &channels)
        .await
        .unwrap();
    let mut total_after_second = 0usize;
    for s in &stores {
        total_after_second += s.lock().await.total();
    }

    // Same CID, no extra shards on nodes.
    assert_eq!(
        manifest_a.data_cid, manifest_b.data_cid,
        "same content → same CID"
    );
    assert_eq!(manifest_a.merkle_root, manifest_b.merkle_root);
    assert_eq!(manifest_a.object_id, manifest_b.object_id);
    assert_eq!(
        total_after_first, total_after_second,
        "deduplication did not work"
    );
}

#[tokio::test]
async fn cid_differs_for_different_content() {
    let gf = Gf::new();
    let (addrs, _stores) = spawn_cluster(N_NODES).await;
    let mut m1 = build_manifest(addrs.clone());
    let mut m2 = build_manifest(addrs);
    let mut ch1 = synth_channels();
    let mut ch2 = synth_channels();
    ch2[0][0] += 5.0; // one difference — should change the hash drastically

    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut m1, &live, &ch1).await.unwrap();
    // sanity: ch1 did not mutate
    assert_eq!(ch1[0][0], synth_channels()[0][0]);
    let _ = &mut ch1;
    put_object(&gf, &mut m2, &live, &ch2).await.unwrap();
    assert_ne!(m1.data_cid, m2.data_cid);
    assert_ne!(m1.object_id, m2.object_id);
}

#[tokio::test]
async fn corrupted_shard_is_rejected_on_read() {
    let gf = Gf::new();
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    // Inject a deliberately bad shard for (c=0, l=0) on node 0.
    let sl = manifest.sym_len[0] as usize;
    let bogus = Shard {
        coeffs: vec![0xDE; K],
        payload: vec![0xAD; sl],
    };
    let bogus_h = shard_hash(&bogus);
    assert!(
        !manifest.shard_hashes[0][0].contains(&bogus_h),
        "generated garbage accidentally collided — astronomically unlikely"
    );
    stores[0]
        .lock()
        .await
        .inject_corrupt((manifest.object_id, 0, 0), bogus);

    let recon = get_object(&gf, &manifest, &live).await.unwrap();
    let p = psnr(&channels, &recon);
    assert!(
        p > 80.0,
        "verifier did not drop the bad shard: PSNR = {p:.1} dB"
    );
}

#[tokio::test]
async fn auth_check_succeeds_with_correct_pubkey_and_fails_with_wrong() {
    // Spin up a real node (spawn_node_full hands back its pubkey).
    // With the right key auth_check passes, with a foreign key it does not.
    let node = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let addr = node.addr.to_string();
    let real_pubkey = node.identity.pubkey();

    assert!(
        auth_check(&addr, &real_pubkey).await,
        "node must authenticate with its own key"
    );

    let attacker = NodeIdentity::generate();
    assert!(
        !auth_check(&addr, &attacker.pubkey()).await,
        "auth must not pass with a foreign key"
    );

    node.task.abort();
}

#[tokio::test]
async fn discover_live_with_whitelist_filters_unsigned_nodes() {
    // 3-node cluster. Put only 2 in the whitelist — the third must drop out.
    let n0 = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let n1 = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let n2 = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let addrs = vec![
        n0.addr.to_string(),
        n1.addr.to_string(),
        n2.addr.to_string(),
    ];

    let admin = NodeIdentity::generate();
    let wl = Whitelist::sign(
        vec![
            WhitelistEntry {
                addr: addrs[0].clone(),
                pubkey: n0.identity.pubkey(),
                zone: 0,
            },
            WhitelistEntry {
                addr: addrs[1].clone(),
                pubkey: n1.identity.pubkey(),
                zone: 0,
            },
            // n2 is missing — must drop out of discover.
        ],
        &admin,
    );
    assert!(wl.verify(Some(&admin.pubkey())));

    let mut manifest = build_manifest(addrs);
    manifest.zones = vec![0; 3];
    let live = discover_live_with_whitelist(&manifest, &wl).await;
    assert_eq!(live, vec![0, 1], "n2 not in whitelist — must drop out");

    n0.task.abort();
    n1.task.abort();
    n2.task.abort();
}

#[tokio::test]
async fn whitelist_rejects_spoofed_node_with_wrong_key() {
    // 2-node cluster. Put a foreign pubkey for node 0 into the whitelist —
    // discover_live_with_whitelist must drop it (handshake fails).
    let n0 = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let n1 = spawn_node_full((Ipv4Addr::LOCALHOST, 0).into())
        .await
        .unwrap();
    let addrs = vec![n0.addr.to_string(), n1.addr.to_string()];

    let admin = NodeIdentity::generate();
    let bogus = NodeIdentity::generate();
    let wl = Whitelist::sign(
        vec![
            WhitelistEntry {
                addr: addrs[0].clone(),
                pubkey: bogus.pubkey(), // ← swapped!
                zone: 0,
            },
            WhitelistEntry {
                addr: addrs[1].clone(),
                pubkey: n1.identity.pubkey(),
                zone: 0,
            },
        ],
        &admin,
    );

    let mut manifest = build_manifest(addrs);
    manifest.zones = vec![0; 2];
    let live = discover_live_with_whitelist(&manifest, &wl).await;
    assert_eq!(live, vec![1], "node 0 is caught by the spoofing check");

    n0.task.abort();
    n1.task.abort();
}

#[tokio::test]
async fn audit_detects_silent_deletion_and_tanks_reputation() {
    // PUT an object across N_NODES, then wipe one node's storage, run
    // targeted audit_shard against that node — expect MissingShard and a
    // reputation drop below the threshold.
    let gf = Gf::new();
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs.clone());
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    // Wipe node 3's contents — simulating "node accepted PUT, then silently deleted".
    stores[3].lock().await.wipe();

    let mut rep = Reputation::new(N_NODES, 1.0);

    // Iterate over c=0, l=0 hashes and hit node 3 specifically for each one.
    // shard_idx projection via place_shard. Those that HRW assigned to node 3
    // must return MissingShard; the rest are either PASS or MissingShard (don't care).
    let mut hits_on_node3 = 0usize;
    let mut misses = 0usize;
    for (idx, hash) in manifest.shard_hashes[0][0].iter().enumerate() {
        let node = manifest.place_shard(0, 0, idx as u32, &live);
        if node != 3 {
            continue;
        }
        hits_on_node3 += 1;
        let outcome = audit_shard(&addrs[3], manifest.object_id, 0, 0, *hash).await;
        let success = outcome.is_success();
        rep.observe(3, success);
        if matches!(outcome, AuditOutcome::MissingShard) {
            misses += 1;
        }
    }
    assert!(
        hits_on_node3 > 0,
        "node 3 must host at least one shard c=0 l=0"
    );
    assert_eq!(
        misses, hits_on_node3,
        "wiped node must return Missing for every one of its hashes"
    );
    assert!(
        rep.score(3) < 0.5,
        "reputation after a MissingShard series must fall below 0.5, actual {:.2}",
        rep.score(3)
    );

    // Node 5 (alive) — PASS, score stays near 1.
    for (idx, hash) in manifest.shard_hashes[0][0].iter().enumerate() {
        let node = manifest.place_shard(0, 0, idx as u32, &live);
        if node != 5 {
            continue;
        }
        let outcome = audit_shard(&addrs[5], manifest.object_id, 0, 0, *hash).await;
        rep.observe(5, outcome.is_success());
        assert!(outcome.is_success(), "alive node must pass");
    }
    assert!(
        rep.score(5) > 0.95,
        "alive node keeps score {:.2}",
        rep.score(5)
    );
}

#[tokio::test]
async fn rebalance_add_node_preserves_decodability() {
    // PUT across N_NODES → add an (N_NODES+1)-th node via rebalance::add_node
    // → repair_node rewrites its HRW share onto the new node → GET still works.
    let gf = Gf::new();
    let mut rng = Rng::new(0xADD_1);
    let (addrs, _stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    // Spin up an extra node and add it to the catalog via add_node.
    let (extra_addr, _extra_store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
    let mut dir = holofs_model::fs::Directory::new();
    dir.insert("obj".into(), manifest.clone());

    let reports = add_node(&gf, &mut rng, &mut dir, extra_addr.to_string(), 0, K).await;
    assert_eq!(reports.len(), 1);
    assert!(reports[0].result.is_ok(), "rebalance failed");

    let manifest_after = dir.get("obj").unwrap().clone();
    assert_eq!(manifest_after.nodes.len(), N_NODES + 1);
    assert_eq!(manifest_after.zones.len(), N_NODES + 1);

    let live_after: Vec<usize> = (0..N_NODES + 1).collect();
    let recon = get_object(&gf, &manifest_after, &live_after).await.unwrap();
    let p = psnr(&channels, &recon);
    assert!(p > 80.0, "decode broken after rebalance: PSNR = {p:.1} dB");
}

#[tokio::test]
async fn corrupted_shard_does_not_taint_repair() {
    let gf = Gf::new();
    let mut rng = Rng::new(0xDEAD);
    let (addrs, stores) = spawn_cluster(N_NODES).await;
    let mut manifest = build_manifest(addrs);
    let channels = synth_channels();
    let live: Vec<usize> = (0..N_NODES).collect();
    put_object(&gf, &mut manifest, &live, &channels)
        .await
        .unwrap();

    // Infect node 5 with a bad shard for (c=0, l=0) — it's a potential donor.
    let sl = manifest.sym_len[0] as usize;
    let bogus = Shard {
        coeffs: vec![0x55; K],
        payload: vec![0xAA; sl],
    };
    stores[5]
        .lock()
        .await
        .inject_corrupt((manifest.object_id, 0, 0), bogus.clone());

    // Kill node 0 and regenerate — repair_node must reject the bad donor.
    stores[0].lock().await.wipe();
    repair_node(&gf, &mut rng, &mut manifest, &live, 0, K)
        .await
        .unwrap();

    let recon = get_object(&gf, &manifest, &live).await.unwrap();
    let p = psnr(&channels, &recon);
    assert!(p > 80.0, "repair pulled in a bad donor: PSNR = {p:.1} dB");
}
