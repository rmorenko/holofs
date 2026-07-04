//! holofs — core demo: encode an image into shards, distribute across nodes,
//! kill some nodes, regenerate as needed, print metrics and PNGs.

use holofs_codec::image_io::{load_photo, save_png, synth, to_rgb};
use holofs_core::cluster::ShardRec;
use holofs_core::gf::Gf;
use holofs_core::repair::{needs_repair, regenerate_node};
use holofs_core::rlnc::{decode_layer, encode_layer, Shard};
use holofs_core::rng::Rng;
use holofs_core::transform::{coeff_layer, haar_forward, haar_inverse};
use holofs_core::{dims_from_env, K, LEVELS, NLAYERS, N_NODES, RED};

fn main() {
    let gf = Gf::new();
    let mut rng = Rng::new(0xC0FFEE);
    let out_dir = std::env::var("HOLOFS_OUT").unwrap_or_else(|_| "out".to_string());
    std::fs::create_dir_all(&out_dir).ok();
    let (w, h) = dims_from_env();
    println!("target size: {w}x{h} (HOLOFS_W/H to override)");

    // Input: PNG path as the first argument. Otherwise — assets/sample.png
    // (Kodak True Color kodim23, public domain). If that is also missing — synthetic.
    let original = match std::env::args().nth(1) {
        Some(path) => {
            println!("source: {path}");
            load_photo(&path, w, h)
        }
        None if std::path::Path::new("assets/sample.png").exists() => {
            println!("source: assets/sample.png (Kodak kodim23, public domain)");
            load_photo("assets/sample.png", w, h)
        }
        None => {
            println!("source: synthetic mandala (no argv, no assets/sample.png)");
            synth(w, h)
        }
    };
    save_png(
        &format!("{out_dir}/00_original.png"),
        w as u32,
        h as u32,
        &to_rgb(&original, w, h),
    );

    let mut layer_positions: Vec<Vec<usize>> = vec![Vec::new(); NLAYERS];
    for y in 0..h {
        for x in 0..w {
            layer_positions[coeff_layer(x, y, w, h)].push(y * w + x);
        }
    }
    let mut n_per_layer = [0usize; NLAYERS];
    for l in 0..NLAYERS {
        n_per_layer[l] = (K as f32 * RED[l]).round() as usize;
    }

    let mut sym_len = [0usize; NLAYERS];
    let mut all: Vec<ShardRec> = Vec::new();
    let mut rr = 0usize;

    for c in 0..3 {
        let mut plane = original[c].clone();
        haar_forward(&mut plane, w, h, LEVELS);
        for l in 0..NLAYERS {
            let mut bytes = Vec::with_capacity(layer_positions[l].len() * 4);
            for &p in &layer_positions[l] {
                bytes.extend_from_slice(&plane[p].to_le_bytes());
            }
            let (sl, shards) = encode_layer(&gf, &bytes, n_per_layer[l], &mut rng);
            sym_len[l] = sl;
            for s in shards {
                all.push(ShardRec {
                    node: rr % N_NODES,
                    channel: c,
                    layer: l,
                    shard: s,
                });
                rr += 1;
            }
        }
    }

    println!("=== holofs: in-process cluster ===");
    println!("nodes: {N_NODES} | image: {w}x{h} | DWT levels: {LEVELS} | threshold K: {K}");
    println!(
        "shards total: {} (~{:.1} per node)\n",
        all.len(),
        all.len() as f32 / N_NODES as f32
    );
    print!("priority layers:  ");
    for l in 0..NLAYERS {
        let tag = if l == 0 {
            "LL/coarse"
        } else {
            "details"
        };
        print!("[{l}:{tag} n={} x{:.2}] ", n_per_layer[l], RED[l]);
    }
    println!("\n");

    let survivals = [100usize, 85, 62, 45, 30];
    let labels = ["s100", "s085", "s062", "s045", "s030"];
    let mut recon_rgbs: Vec<Vec<u8>> = Vec::new();

    println!(
        "{:>6} {:>5} {:>8}   layers L0 L1 L2 L3 (v ok / . lost)",
        "alive", "dead", "PSNR"
    );
    for (si, &surv) in survivals.iter().enumerate() {
        let alive = (N_NODES * surv + 99) / 100;
        let dead = N_NODES - alive;
        let mut recon = [vec![0f32; w * h], vec![0f32; w * h], vec![0f32; w * h]];
        let mut layer_ok = [true; NLAYERS];

        for c in 0..3 {
            let mut plane = vec![0f32; w * h];
            for l in 0..NLAYERS {
                let live: Vec<&Shard> = all
                    .iter()
                    .filter(|r| r.channel == c && r.layer == l && r.node < alive)
                    .map(|r| &r.shard)
                    .collect();
                match decode_layer(&gf, &live, sym_len[l]) {
                    Some(bytes) => {
                        for (idx, &p) in layer_positions[l].iter().enumerate() {
                            let b = &bytes[idx * 4..idx * 4 + 4];
                            plane[p] = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                        }
                    }
                    None => {
                        layer_ok[l] = false;
                    }
                }
            }
            haar_inverse(&mut plane, w, h, LEVELS);
            recon[c] = plane;
        }

        let mut mse = 0f64;
        for c in 0..3 {
            for i in 0..w * h {
                let d = (recon[c][i] - original[c][i]) as f64;
                mse += d * d;
            }
        }
        mse /= (3 * w * h) as f64;
        let psnr = if mse < 1e-9 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        };

        let rgb = to_rgb(&recon, w, h);
        save_png(
            &format!("{out_dir}/{:02}_{}.png", si + 1, labels[si]),
            w as u32,
            h as u32,
            &rgb,
        );
        recon_rgbs.push(rgb);

        let mut marks = String::new();
        for l in 0..NLAYERS {
            marks.push(if layer_ok[l] { 'v' } else { '.' });
            marks.push(' ');
        }
        println!("{:>5}% {:>4}  {:>7.1}dB   {}", surv, dead, psnr, marks);
    }

    let cols = 3usize;
    let rows_n = 2usize;
    let gap = 6usize;
    let mw = cols * w + (cols + 1) * gap;
    let mh = rows_n * h + (rows_n + 1) * gap;
    let mut montage = vec![20u8; mw * mh * 3];
    let orig_rgb = to_rgb(&original, w, h);
    let mut cells: Vec<&Vec<u8>> = vec![&orig_rgb];
    for r in &recon_rgbs {
        cells.push(r);
    }
    for (ci, cell) in cells.iter().enumerate() {
        let cr = ci / cols;
        let cc = ci % cols;
        let ox = gap + cc * (w + gap);
        let oy = gap + cr * (h + gap);
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 3;
                let dst = ((oy + y) * mw + (ox + x)) * 3;
                montage[dst..dst + 3].copy_from_slice(&cell[src..src + 3]);
            }
        }
    }
    save_png(
        &format!("{out_dir}/montage.png"),
        mw as u32,
        mh as u32,
        &montage,
    );
    println!("\nmontage: montage.png (order: original, s100, s085, s062, s045, s030)");

    println!("\n=== RLNC regeneration (repair without full reconstruction) ===");
    let d_param = K;
    let threshold = 0.5_f32;
    let kill_count = (N_NODES * 60) / 100;
    println!(
        "params: d = K = {K}, layer margin threshold = {:.0}%, death series = {kill_count}",
        threshold * 100.0
    );

    let mut all_rep = all.clone();
    let mut traffic_repair = 0u64;
    let mut traffic_baseline = 0u64;
    let mut muls_repair = 0u64;
    let mut muls_baseline = 0u64;
    let mut repairs = 0usize;
    let mut shards_regen = 0usize;
    let mut unrecov = 0usize;

    println!("\n{:>4} {:>6}   event", "i", "node");
    for i in 0..kill_count {
        let dead = i % N_NODES;
        let had = all_rep.iter().filter(|r| r.node == dead).count();
        all_rep.retain(|r| r.node != dead);

        if needs_repair(&all_rep, &n_per_layer, threshold) {
            let st = regenerate_node(
                &gf,
                dead,
                &mut all_rep,
                &sym_len,
                &n_per_layer,
                d_param,
                &mut rng,
            );
            traffic_repair += st.bytes_downloaded;
            traffic_baseline += st.bytes_baseline_full;
            muls_repair += st.gf_muls_repair;
            muls_baseline += st.gf_muls_baseline;
            shards_regen += st.shards_generated;
            unrecov += st.layers_unrecoverable;
            repairs += 1;
            println!(
                "{:>4} {:>6}   death (-{had} shards) → repair: +{} shards, {} bytes downloaded",
                i, dead, st.shards_generated, st.bytes_downloaded
            );
        } else {
            println!(
                "{:>4} {:>6}   death (-{had} shards), layer margin still ok — repair skipped",
                i, dead
            );
        }
    }

    let mut recon = [vec![0f32; w * h], vec![0f32; w * h], vec![0f32; w * h]];
    let mut all_layers_ok = true;
    for c in 0..3 {
        let mut plane = vec![0f32; w * h];
        for l in 0..NLAYERS {
            let live: Vec<&Shard> = all_rep
                .iter()
                .filter(|r| r.channel == c && r.layer == l)
                .map(|r| &r.shard)
                .collect();
            match decode_layer(&gf, &live, sym_len[l]) {
                Some(bytes) => {
                    for (idx, &p) in layer_positions[l].iter().enumerate() {
                        let b = &bytes[idx * 4..idx * 4 + 4];
                        plane[p] = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    }
                }
                None => {
                    all_layers_ok = false;
                }
            }
        }
        haar_inverse(&mut plane, w, h, LEVELS);
        recon[c] = plane;
    }
    let mut mse = 0f64;
    for c in 0..3 {
        for i in 0..w * h {
            let dd = (recon[c][i] - original[c][i]) as f64;
            mse += dd * dd;
        }
    }
    mse /= (3 * w * h) as f64;
    let psnr_rep = if mse < 1e-9 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };

    println!("\nsummary:");
    println!(
        "  repairs: {repairs} | regenerated shards: {shards_regen} | layer losses: {unrecov}"
    );
    println!(
        "  repair traffic:   {:>11} bytes ({:.2} MB)",
        traffic_repair,
        traffic_repair as f64 / 1.0e6
    );
    println!(
        "  baseline (full):  {:>11} bytes ({:.2} MB)",
        traffic_baseline,
        traffic_baseline as f64 / 1.0e6
    );
    println!("  GF mults (repair):    {:>13}", muls_repair);
    println!("  GF mults (baseline):  {:>13}", muls_baseline);
    if muls_baseline > 0 {
        let save = 1.0 - muls_repair as f64 / muls_baseline as f64;
        println!("  CPU saved (GF-mul):  {:.1}%", save * 100.0);
    }
    println!(
        "  PSNR of object after series: {:.1} dB  (all layers decode: {})",
        psnr_rep, all_layers_ok
    );
    save_png(
        &format!("{out_dir}/repaired.png"),
        w as u32,
        h as u32,
        &to_rgb(&recon, w, h),
    );
    println!("  output: repaired.png");

    // Control: same deaths WITHOUT repair.
    let mut all_noheal = all.clone();
    for i in 0..kill_count {
        let dead = i % N_NODES;
        all_noheal.retain(|r| r.node != dead);
    }
    let mut recon_nh = [vec![0f32; w * h], vec![0f32; w * h], vec![0f32; w * h]];
    let mut ok_nh = [[true; NLAYERS]; 3];
    for c in 0..3 {
        let mut plane = vec![0f32; w * h];
        for l in 0..NLAYERS {
            let live: Vec<&Shard> = all_noheal
                .iter()
                .filter(|r| r.channel == c && r.layer == l)
                .map(|r| &r.shard)
                .collect();
            match decode_layer(&gf, &live, sym_len[l]) {
                Some(bytes) => {
                    for (idx, &p) in layer_positions[l].iter().enumerate() {
                        let b = &bytes[idx * 4..idx * 4 + 4];
                        plane[p] = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    }
                }
                None => {
                    ok_nh[c][l] = false;
                }
            }
        }
        haar_inverse(&mut plane, w, h, LEVELS);
        recon_nh[c] = plane;
    }
    let mut mse_nh = 0f64;
    for c in 0..3 {
        for i in 0..w * h {
            let dd = (recon_nh[c][i] - original[c][i]) as f64;
            mse_nh += dd * dd;
        }
    }
    mse_nh /= (3 * w * h) as f64;
    let psnr_nh = if mse_nh < 1e-9 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse_nh).log10()
    };
    let mut lost_layers = 0;
    for c in 0..3 {
        for l in 0..NLAYERS {
            if !ok_nh[c][l] {
                lost_layers += 1;
            }
        }
    }
    println!("\ncontrol without repair (same {kill_count} deaths):");
    println!(
        "  PSNR: {:.1} dB | lost (channel,layer) pairs: {lost_layers}/{}",
        psnr_nh,
        3 * NLAYERS
    );
}
