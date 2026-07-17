//! Demo of the distributed layer in a single process:
//! spin up N nodes as tokio tasks, encode an image, distribute across nodes,
//! read back, kill nodes, repair, read again.
//!
//! Run: `cargo run --release --bin holofs-cluster`.

use std::net::Ipv4Addr;

use holofs_client::{discover_live, get_object, put_object, repair_node};
use holofs_codec::image_io::{load_photo, save_png, synth, to_rgb};
use holofs_core::gf::Gf;
use holofs_core::hash::hex;
use holofs_core::merkle::shard_hash;
use holofs_core::rlnc::Shard;
use holofs_core::rng::Rng;
use holofs_core::transform::coeff_layer;
use holofs_core::{dims_from_env, K, LEVELS, NLAYERS, N_NODES, RED};
use holofs_model::manifest::{Manifest, ManifestState};
use holofs_model::placement::Placement;
use holofs_storage::node_service::{spawn_node, SharedStore};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gf = Gf::new();
    let mut rng = Rng::new(0xCAFE_BABE);
    let out_dir = std::env::var("HOLOFS_OUT").unwrap_or_else(|_| "out".to_string());
    std::fs::create_dir_all(&out_dir).ok();
    let (w, h) = dims_from_env();
    println!("target size: {w}x{h} (HOLOFS_W/H to override)");

    // === Bring up N_NODES nodes on random ports ===========================
    let mut node_addrs: Vec<String> = Vec::with_capacity(N_NODES);
    let mut stores: Vec<SharedStore> = Vec::with_capacity(N_NODES);
    for _ in 0..N_NODES {
        let (bound, store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await?;
        node_addrs.push(bound.to_string());
        stores.push(store);
    }
    println!("cluster up: {N_NODES} nodes on localhost");

    // === Compute layer_positions / n_per_layer / sym_len, like the in-memory demo ===
    let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
    for y in 0..h {
        for x in 0..w {
            layer_positions[coeff_layer(x, y, w, h)].push((y * w + x) as u32);
        }
    }
    let n_per_layer: Vec<u32> = (0..NLAYERS)
        .map(|l| (K as f32 * RED[l]).round() as u32)
        .collect();
    let sym_len: Vec<u32> = n_per_layer
        .iter()
        .zip(layer_positions.iter())
        .map(|(_, pos)| {
            // sym_len = ceil(layer_bytes / K), layer_bytes = positions.len() * 4
            let bytes = pos.len() * 4;
            ((bytes + K - 1) / K) as u32
        })
        .collect();

    // object_id and hashes will be filled by put_object — placeholders here.
    let mut manifest = Manifest {
        object_id: 0,
        k: K as u16,
        nlayers: NLAYERS as u8,
        n_per_layer: n_per_layer.clone(),
        sym_len: sym_len.clone(),
        layer_positions: layer_positions.clone(),
        channels: 3,
        width: w as u32,
        height: h as u32,
        levels: LEVELS as u8,
        nodes: node_addrs.clone(),
        placement: Placement::Rendezvous,
        zones: vec![0; node_addrs.len()],
        data_cid: [0; 32],
        merkle_root: [0; 32],
        shard_hashes: vec![vec![Vec::new(); NLAYERS]; 3],
        kind: holofs_model::manifest::ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: 0,
        encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
            retention: None,
        };

    // === Prepare an image: argv → assets/sample.png → synthetic ===========
    let channels = match std::env::args().nth(1) {
        Some(path) => {
            println!("source: {path}");
            let arr = load_photo(&path, w, h);
            vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
        }
        None if std::path::Path::new("assets/sample.png").exists() => {
            println!("source: assets/sample.png (Kodak kodim23, public domain)");
            let arr = load_photo("assets/sample.png", w, h);
            vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
        }
        None => {
            println!("source: synthetic mandala (no argv, no assets/sample.png)");
            let arr = synth(w, h);
            vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
        }
    };
    save_png(
        &format!("{out_dir}/cluster_00_original.png"),
        w as u32,
        h as u32,
        &to_rgb_arr(&channels, w, h),
    );

    // === PUT ===============================================================
    let live = discover_live(&manifest).await;
    println!("live nodes before PUT: {}/{N_NODES}", live.len());
    put_object(&gf, &mut manifest, &live, &channels).await?;
    println!(
        "manifest: {} bytes; placement = Rendezvous; n_per_layer = {:?}",
        manifest.encode().len(),
        manifest.n_per_layer
    );
    println!("data_cid  : {}", hex(&manifest.data_cid));
    println!("merkle_root: {}", hex(&manifest.merkle_root));
    println!("object_id : {:#018x}  (from data_cid)", manifest.object_id);

    // Total shard count across nodes.
    let mut total = 0usize;
    for s in &stores {
        total += s.lock().await.total();
    }
    println!("PUT done: distributed {total} shards");

    // === Deduplication: repeat PUT of the same object ======================
    let mut manifest_dup = manifest.clone();
    manifest_dup.data_cid = [0; 32];
    manifest_dup.merkle_root = [0; 32];
    manifest_dup.shard_hashes = vec![vec![Vec::new(); NLAYERS]; 3];
    put_object(&gf, &mut manifest_dup, &live, &channels).await?;
    let mut total_after = 0usize;
    for s in &stores {
        total_after += s.lock().await.total();
    }
    println!(
        "repeat PUT of the same image: data_cid {} → cluster shards: {total_after} \
         (dedup {})",
        if manifest_dup.data_cid == manifest.data_cid {
            "matched"
        } else {
            "DIVERGED"
        },
        if total_after == total {
            "worked"
        } else {
            "broken"
        }
    );

    // === GET (full) =======================================================
    let recon_full = get_object(&gf, &manifest, &live).await?;
    save_png(
        &format!("{out_dir}/cluster_01_get.png"),
        w as u32,
        h as u32,
        &to_rgb_arr(&recon_full, w, h),
    );
    println!(
        "GET after PUT: PSNR = {:.1} dB",
        psnr(&channels, &recon_full)
    );

    // === Corruption: inject a "garbage" shard, catch it by hash ============
    // Take the first node and for the pair (c=0, l=0) inject a shard with
    // broken payload — its hash is not in manifest.shard_hashes, so the
    // verifier must drop it.
    let bogus = Shard {
        coeffs: vec![0xDE; K],
        payload: vec![0xAD; sym_len[0] as usize],
    };
    stores[0]
        .lock()
        .await
        .inject_corrupt((manifest.object_id, 0, 0), bogus.clone());
    let bogus_hash = shard_hash(&bogus);
    let bogus_in_manifest = manifest.shard_hashes[0][0].iter().any(|h| h == &bogus_hash);
    println!(
        "\ncorruption: bad shard injected on node 0 (hash in manifest: {})",
        if bogus_in_manifest {
            "accidentally matched"
        } else {
            "no — must be dropped"
        }
    );
    let recon_after_corrupt = get_object(&gf, &manifest, &live).await?;
    println!(
        "GET after the bad shard injection: PSNR = {:.1} dB (verifier worked)",
        psnr(&channels, &recon_after_corrupt)
    );

    // === Progressive death + repair =======================================
    // The narrow L3 layer has n=18 with K=16 — mass simultaneous death
    // quickly drops the rank below K and repair loses donors. We follow
    // kill one → repair → kill next.
    let kill_count = (N_NODES * 50) / 100;
    let live_all: Vec<usize> = (0..N_NODES).collect();
    let mut total_traffic = 0u64;
    let mut total_baseline = 0u64;
    let mut total_muls_r = 0u64;
    let mut total_muls_b = 0u64;
    let mut total_regen = 0usize;

    println!("\nprogressive scenario: kill {kill_count} nodes one by one; repair after each death");
    for i in 0..kill_count {
        stores[i].lock().await.wipe();
        let st = repair_node(&gf, &mut rng, &mut manifest, &live_all, i, K).await?;
        total_traffic += st.bytes_downloaded;
        total_baseline += st.bytes_baseline_full;
        total_muls_r += st.gf_muls_repair;
        total_muls_b += st.gf_muls_baseline;
        total_regen += st.shards_generated;
        if i < 3 || i == kill_count - 1 {
            println!(
                "  i={i:>2}: killed/repaired node {i} (+{} shards, downloaded {} bytes)",
                st.shards_generated, st.bytes_downloaded
            );
        } else if i == 3 {
            println!("  ... (identical steps omitted)");
        }
    }

    println!(
        "\nrepair done: regenerated {total_regen} shards; traffic {} bytes ({:.2} MB)",
        total_traffic,
        total_traffic as f64 / 1.0e6
    );
    println!(
        "baseline (full reconstruction): {} bytes ({:.2} MB)",
        total_baseline,
        total_baseline as f64 / 1.0e6
    );
    if total_muls_b > 0 {
        let saved = 1.0 - total_muls_r as f64 / total_muls_b as f64;
        println!(
            "GF-mul: repair {} vs baseline {}  ⇒ CPU saved {:.1}%",
            total_muls_r,
            total_muls_b,
            saved * 100.0
        );
    }

    // === GET after progressive repair =====================================
    let recon_repaired = get_object(&gf, &manifest, &live_all).await?;
    save_png(
        &format!("{out_dir}/cluster_02_repaired.png"),
        w as u32,
        h as u32,
        &to_rgb_arr(&recon_repaired, w, h),
    );
    println!(
        "GET after {} death+repair cycles: PSNR = {:.1} dB",
        kill_count,
        psnr(&channels, &recon_repaired)
    );

    Ok(())
}

fn psnr(orig: &[Vec<f32>], recon: &[Vec<f32>]) -> f64 {
    let n = orig.len() * orig[0].len();
    let mut mse = 0f64;
    for c in 0..orig.len() {
        for i in 0..orig[c].len() {
            let d = (orig[c][i] - recon[c][i]) as f64;
            mse += d * d;
        }
    }
    mse /= n as f64;
    if mse < 1e-9 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn to_rgb_arr(v: &[Vec<f32>], w: usize, h: usize) -> Vec<u8> {
    let arr: [Vec<f32>; 3] = [v[0].clone(), v[1].clone(), v[2].clone()];
    to_rgb(&arr, w, h)
}
