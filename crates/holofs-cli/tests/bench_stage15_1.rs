//! sizing-rule bench harness.
//!
//! The rollback was caused by shipping an encoding that
//! produced ~786k shards per PUT before anyone benchmarked it —
//! the disk-backed store pinned at 99% CPU and the PUT never
//! returned. This harness is the guardrail against a repeat: it
//! runs the 512×512 Replicated PUT + GET + spotlight-ROI cycle
//! across a matrix of `block_size` values and prints a table of
//! (shards, put_ms, full_get_ms, roi_get_ms, roi_bytes / full_bytes).
//!
//! Ignored by default — takes ~30 s in release. Run with:
//!
//! ```sh
//! cargo test --release -p holofs-cli --test bench_stage15_1 \
//!     -- --ignored --nocapture
//! ```
//!
//! Read the printed table before merging any encoding change; if
//! either `put_ms` or `shards` regresses more than 2× on a given
//! `block_size`, that's a strong signal of another 15.0-style
//! trap. `roi_bytes / full_bytes` under `1%` for the 16×16 corner
//! ROI is the "bandwidth-aware spotlight actually works" gate.

use std::net::Ipv4Addr;

use holofs_client::{
    discover_live, get_object_blocks, put_object_replicated_blocks, LiveNodes,
};
use holofs_core::transform::roi_to_block_ids_with_stride;
use holofs_core::K;
use holofs_model::manifest::{Manifest, ObjectEncoding, ObjectKind};
use holofs_model::placement::Placement;
use holofs_storage::node_service::spawn_node;

const W: usize = 512;
const H: usize = 512;
const LEVELS: usize = 3;
const NLAYERS: usize = LEVELS + 1;
const N_NODES: usize = 8;
const REPLICATION: u8 = 3;
const ROI: (usize, usize, usize, usize) = (0, 0, 16, 16);

fn layer_of(x: usize, y: usize) -> usize {
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

async fn spawn_cluster(n: usize) -> Vec<String> {
    let mut addrs = Vec::with_capacity(n);
    for _ in 0..n {
        let (a, _s, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn node");
        addrs.push(a.to_string());
    }
    addrs
}

fn build_manifest(nodes: Vec<String>) -> Manifest {
    let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
    for y in 0..H {
        for x in 0..W {
            layer_positions[layer_of(x, y)].push((y * W + x) as u32);
        }
    }
    let n_nodes = nodes.len();
    Manifest {
        object_id: 0,
        k: K as u16,
        nlayers: NLAYERS as u8,
        n_per_layer: vec![0; NLAYERS],
        sym_len: vec![0; NLAYERS],
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
        kind: ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: 0,
        encoding: ObjectEncoding::Rlnc,
    }
}

fn synth_noisy() -> Vec<Vec<f32>> {
    let mut rng_state: u64 = 0xDEAD_BEEF_1234_5678;
    let mut noise = || -> f32 {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        (rng_state as i64 as f32 / i64::MAX as f32) * 25.0
    };
    let mut ch = vec![vec![0f32; W * H]; 3];
    for y in 0..H {
        for x in 0..W {
            let i = y * W + x;
            ch[0][i] = 128.0 + 50.0 * (x as f32 * 0.03).sin() + noise();
            ch[1][i] = 128.0 + 50.0 * (y as f32 * 0.03).cos() + noise();
            ch[2][i] = 128.0 + 50.0 * ((x + y) as f32 * 0.02).sin() + noise();
        }
    }
    ch
}

async fn bench_one(
    live: &LiveNodes,
    addrs: &[String],
    channels: &[Vec<f32>],
    block_size: u32,
) -> Row {
    let mut manifest = build_manifest(addrs.to_vec());

    let t_put = std::time::Instant::now();
    put_object_replicated_blocks(&mut manifest, live, channels, block_size, REPLICATION)
        .await
        .expect("put_object_replicated_blocks");
    let put_ms = t_put.elapsed().as_millis();

    let blocks: u64 = manifest.n_per_layer.iter().map(|&n| n as u64).sum::<u64>()
        * manifest.channels as u64;
    let shards = blocks * REPLICATION as u64;

    let all_ids: Vec<Vec<u32>> = manifest
        .n_per_layer
        .iter()
        .map(|&n| (0..n).collect())
        .collect();
    let t_full = std::time::Instant::now();
    let (_recon, bytes_full) = get_object_blocks(&manifest, live, &all_ids)
        .await
        .expect("full get_object_blocks");
    let full_get_ms = t_full.elapsed().as_millis();

    let roi_ids = roi_to_block_ids_with_stride(
        ROI.0,
        ROI.1,
        ROI.2,
        ROI.3,
        W,
        H,
        &manifest.layer_positions,
        block_size as usize,
    );
    let t_roi = std::time::Instant::now();
    let (_recon_roi, bytes_roi) = get_object_blocks(&manifest, live, &roi_ids)
        .await
        .expect("roi get_object_blocks");
    let roi_get_ms = t_roi.elapsed().as_millis();

    Row {
        block_size,
        blocks,
        shards,
        put_ms,
        full_get_ms,
        roi_get_ms,
        bytes_full,
        bytes_roi,
    }
}

struct Row {
    block_size: u32,
    blocks: u64,
    shards: u64,
    put_ms: u128,
    full_get_ms: u128,
    roi_get_ms: u128,
    bytes_full: u64,
    bytes_roi: u64,
}

/// The bench. Ignored by default so it doesn't burn 30 s of every
/// CI run — invoke explicitly (see file-level doc). Assertion at
/// the end pins the "bandwidth-aware ROI" property on the small
/// block sizes so a code change that quietly disables the ROI
/// path fails loud instead of just showing a worse table.
#[tokio::test]
#[ignore]
async fn bench_stage15_1_sizing_rule() {
    let addrs = spawn_cluster(N_NODES).await;
    let channels = synth_noisy();
    let manifest = build_manifest(addrs.clone());
    let live = discover_live(&manifest).await;
    assert_eq!(live.len(), N_NODES);

    let sizes: [u32; 5] = [32, 64, 128, 256, 512];
    let mut rows = Vec::with_capacity(sizes.len());
    for &bs in &sizes {
        rows.push(bench_one(&live, &addrs, &channels, bs).await);
    }

    // Print the table. `--nocapture` on the test command line is
    // what surfaces this to the terminal.
    println!(
        "\n{:>10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "block_sz", "blocks", "shards", "put_ms", "full_get_ms", "roi_get_ms", "bytes_full", "bytes_roi"
    );
    println!("{}", "-".repeat(96));
    for r in &rows {
        println!(
            "{:>10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>12} {:>12}",
            r.block_size,
            r.blocks,
            r.shards,
            r.put_ms,
            r.full_get_ms,
            r.roi_get_ms,
            r.bytes_full,
            r.bytes_roi
        );
    }
    let ratios: Vec<f64> = rows
        .iter()
        .map(|r| r.bytes_roi as f64 / r.bytes_full as f64)
        .collect();
    println!(
        "{:>10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "-",
        "-",
        "-",
        "-",
        "-",
        "-",
        "-",
        format!(
            "roi/full: [{}]",
            ratios
                .iter()
                .map(|f| format!("{:.3}", f))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    // Guard rails — fail the bench if the sizing story regresses.
    for r in &rows {
        // 512×512 × 3 channels × R=3 = at most 786432 * 3 = 2.36M
        // shards, but that's what killed 15.0. Anything under 200k
        // is safe (block_size=32 → 24576 blocks × 3 = 73728 shards).
        assert!(
            r.shards < 200_000,
            "block_size={} exceeded safe shard budget: {} shards",
            r.block_size,
            r.shards
        );
        // Every combo must beat 30 s in release (that's the "no
        // catastrophic regression" threshold; typical is < 1 s).
        assert!(
            r.put_ms < 30_000,
            "block_size={} PUT took {} ms — regression?",
            r.block_size,
            r.put_ms
        );
    }
    // The small-block combos MUST deliver bandwidth-aware ROI.
    // At bs=32 or 64 a 16×16 corner should fetch < 5% of the full
    // image bytes (typical is much lower; 5% is the safety margin).
    for r in rows.iter().filter(|r| r.block_size <= 64) {
        let ratio = r.bytes_roi as f64 / r.bytes_full as f64;
        assert!(
            ratio < 0.05,
            "block_size={} ROI/full ratio {:.4} above 5% cap — spotlight is not saving bandwidth",
            r.block_size,
            ratio
        );
    }
}
