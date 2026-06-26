//! Demo of the file interface (Stage 4):
//! - place several files into the catalog
//! - show progressive reads (preview → details → full)
//! - print time and traffic at each stage
//!
//! Run: `cargo run --release --bin holofs-fs [PATH_TO_PNG]`.
//! Without an argument assets/sample.png is used.

use std::net::Ipv4Addr;
use std::time::Instant;

use holofs_client::{get_object_up_to_layer, put_object};
use holofs_codec::image_io::{load_photo, save_png, synth, to_rgb};
use holofs_core::gf::Gf;
use holofs_core::hash::hex;
use holofs_core::transform::coeff_layer;
use holofs_core::{dims_from_env, K, LEVELS, NLAYERS, N_NODES, RED};
use holofs_model::fs::Directory;
use holofs_model::manifest::Manifest;
use holofs_model::placement::Placement;
use holofs_storage::node_service::{spawn_node, SharedStore};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gf = Gf::new();
    let out_dir = std::env::var("HOLOFS_OUT").unwrap_or_else(|_| "out".to_string());
    std::fs::create_dir_all(&out_dir).ok();
    let (w, h) = dims_from_env();

    // === Spin up the cluster ==============================================
    let mut node_addrs: Vec<String> = Vec::with_capacity(N_NODES);
    let mut stores: Vec<SharedStore> = Vec::with_capacity(N_NODES);
    for _ in 0..N_NODES {
        let (bound, store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await?;
        node_addrs.push(bound.to_string());
        stores.push(store);
    }
    println!("cluster: {N_NODES} nodes, frame size {w}x{h}");

    // === Prepare two files: a real photo and a synthetic mandala ==========
    let photo_path = std::env::args().nth(1).unwrap_or_else(|| {
        if std::path::Path::new("assets/sample.png").exists() {
            "assets/sample.png".to_string()
        } else {
            "".to_string()
        }
    });
    let photo_channels = if photo_path.is_empty() {
        println!("no argv and no assets/sample.png — both files will be synthetic");
        let arr = synth(w, h);
        vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
    } else {
        println!("file: photo.png ← {photo_path}");
        let arr = load_photo(&photo_path, w, h);
        vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
    };
    let mandala_channels = {
        let arr = synth(w, h);
        vec![arr[0].clone(), arr[1].clone(), arr[2].clone()]
    };

    // === Catalog: name → manifest =========================================
    let mut directory = Directory::new();
    let live: Vec<usize> = (0..N_NODES).collect();

    for (name, channels) in [
        ("photo.png", &photo_channels),
        ("mandala.png", &mandala_channels),
    ] {
        let mut manifest = blank_manifest(&node_addrs, w, h);
        put_object(&gf, &mut manifest, &live, channels).await?;
        println!(
            "PUT {name:<14} CID={}  object {}x{}",
            hex(&manifest.data_cid[..8]),
            manifest.width,
            manifest.height
        );
        directory.insert(name.to_string(), manifest);
    }

    // Serialize and write the catalog next to the PNGs — this is the
    // "directory on disk".
    let cat_bytes = directory.encode();
    let cat_path = format!("{out_dir}/fs_catalog.bin");
    std::fs::write(&cat_path, &cat_bytes)?;
    println!(
        "\ncatalog: {cat_path} ({} entries, {} bytes)",
        directory.len(),
        cat_bytes.len()
    );

    // === ls =================================================================
    println!("\n$ holofs-ls");
    for name in directory.names() {
        let m = directory.get(&name).unwrap();
        let n_shards: u32 = m.n_per_layer.iter().sum::<u32>() * m.channels as u32;
        println!(
            "  {name:<14}  {}x{}  {} shards  CID={}",
            m.width,
            m.height,
            n_shards,
            hex(&m.data_cid[..8])
        );
    }

    // === Progressive read =================================================
    println!("\n=== Progressive read of photo.png ===");
    println!("concept: coarse layers are fetched first — instant preview;");
    println!("details (fine frequencies) are pulled as needed.\n");
    let manifest = directory.get("photo.png").unwrap();

    let mut last_recon = None;
    for max_layer in 0..NLAYERS as u8 {
        let t0 = Instant::now();
        let (recon, bytes) = get_object_up_to_layer(&gf, manifest, &live, max_layer).await?;
        let elapsed = t0.elapsed();
        let p = if let Some(orig) = Some(&photo_channels) {
            psnr(orig, &recon)
        } else {
            0.0
        };
        let label = format!("L0..L{max_layer}");
        println!(
            "  stage {label:<7}  {:>4} ms  downloaded {:>8} bytes ({:>5.1} KB)  PSNR {:.1} dB",
            elapsed.as_millis(),
            bytes,
            bytes as f64 / 1024.0,
            p
        );
        let png = format!("{out_dir}/fs_progressive_L0_to_L{max_layer}.png");
        save_png(&png, w as u32, h as u32, &to_rgb_arr(&recon, w, h));
        last_recon = Some(recon);
    }

    // Save the original for comparison.
    save_png(
        &format!("{out_dir}/fs_00_original.png"),
        w as u32,
        h as u32,
        &to_rgb_arr(&photo_channels, w, h),
    );

    // === Montage ===========================================================
    let mut grid: Vec<Vec<u8>> = Vec::new();
    grid.push(to_rgb_arr(&photo_channels, w, h));
    for max_layer in 0..NLAYERS as u8 {
        let png = format!("{out_dir}/fs_progressive_L0_to_L{max_layer}.png");
        let bytes = std::fs::read(&png)?;
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        grid.push(buf);
    }
    let cols = grid.len();
    let gap = 6usize;
    let mw = cols * w + (cols + 1) * gap;
    let mh = h + 2 * gap;
    let mut montage = vec![20u8; mw * mh * 3];
    for (ci, cell) in grid.iter().enumerate() {
        let ox = gap + ci * (w + gap);
        let oy = gap;
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 3;
                let dst = ((oy + y) * mw + (ox + x)) * 3;
                montage[dst..dst + 3].copy_from_slice(&cell[src..src + 3]);
            }
        }
    }
    save_png(
        &format!("{out_dir}/fs_montage.png"),
        mw as u32,
        mh as u32,
        &montage,
    );
    println!("\nmontage: out/fs_montage.png  (original | L0 | L0-L1 | L0-L1-L2 | full)");

    // Suppress unused warning.
    let _ = last_recon;
    Ok(())
}

fn blank_manifest(node_addrs: &[String], w: usize, h: usize) -> Manifest {
    let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
    for y in 0..h {
        for x in 0..w {
            layer_positions[coeff_layer(x, y, w, h)].push((y * w + x) as u32);
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
    Manifest {
        object_id: 0,
        k: K as u16,
        nlayers: NLAYERS as u8,
        n_per_layer,
        sym_len,
        layer_positions,
        channels: 3,
        width: w as u32,
        height: h as u32,
        levels: LEVELS as u8,
        nodes: node_addrs.to_vec(),
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
    }
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
