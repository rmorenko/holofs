//! Look inside: what is physically stored on a node as one shard.
//!
//! Runs encode on a single layer (LL = coarse structure), pulls out shard 0
//! (systematic, `coeffs = e_0`) and shard K (first RLNC, `coeffs` = random
//! bytes), prints them in hex, and saves the payload as a grayscale PNG so
//! you can see it visually:
//! - systematic — structured noise (raw f32 bytes of wavelet coefficients;
//!   little-endian f32 banding is visible);
//! - RLNC — uniform noise (information-theoretic "mush").
//!
//! Run: `cargo run --release --bin holofs-inspect`.
//! Output: `out/shard_systematic.png`, `out/shard_rlnc.png`.

use holofs_codec::image_io::{load_photo, save_png, synth};
use holofs_core::gf::Gf;
use holofs_core::rlnc::encode_layer;
use holofs_core::rng::Rng;
use holofs_core::transform::{coeff_layer, haar_forward};
use holofs_core::{K, LEVELS};

fn main() {
    let gf = Gf::new();
    let (w, h) = (512usize, 512usize);

    // Take an image, run DWT, pick layer 0 (LL — coarse structure).
    let channels = if std::path::Path::new("assets/sample.png").exists() {
        let a = load_photo("assets/sample.png", w, h);
        vec![a[0].clone(), a[1].clone(), a[2].clone()]
    } else {
        let a = synth(w, h);
        vec![a[0].clone(), a[1].clone(), a[2].clone()]
    };

    // DWT of the first channel.
    let mut plane = channels[0].clone();
    haar_forward(&mut plane, w, h, LEVELS);

    // Collect layer 0 bytes: LL block positions → flat f32 array → bytes.
    let mut layer0_positions: Vec<u32> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if coeff_layer(x, y, w, h) == 0 {
                layer0_positions.push((y * w + x) as u32);
            }
        }
    }
    let mut bytes = Vec::with_capacity(layer0_positions.len() * 4);
    for &p in &layer0_positions {
        bytes.extend_from_slice(&plane[p as usize].to_le_bytes());
    }

    println!("layer 0 (LL, coarse structure of channel R):");
    println!("  pixels in layer: {}", layer0_positions.len());
    println!("  raw bytes (as f32 LE): {}", bytes.len());

    // Encode into 2K shards (K=16 systematic + K=16 RLNC).
    let mut rng = Rng::new(0xC0FFEE);
    let n = 2 * K;
    let (symbol_len, shards) = encode_layer(&gf, &bytes, n, &mut rng);

    println!("  K = {K},  n_shards = {n},  symbol_len = {symbol_len} bytes",);
    println!(
        "  one shard size = K(coeffs) + symbol_len = {} + {} = {} bytes\n",
        K,
        symbol_len,
        K + symbol_len,
    );

    // === Systematic shard (0) ===
    let s0 = &shards[0];
    println!("=== SHARD 0 (systematic, e_0) ===");
    println!("coeffs ({} bytes):", s0.coeffs.len());
    println!("  {}", hex(&s0.coeffs));
    println!("payload ({} bytes), first 64:", s0.payload.len());
    println!("  {}", hex(&s0.payload[..64]));
    println!(
        "  ↑ these are the raw bytes of {} DWT f32 coefficients in little-endian; \
         zeros in high bytes are zero mantissa/exponent bytes of small numbers",
        symbol_len / 4
    );
    println!();

    // === RLNC shard (K = first non-systematic) ===
    let sk = &shards[K];
    println!("=== SHARD {K} (RLNC, random linear combination) ===");
    println!("coeffs ({} bytes):", sk.coeffs.len());
    println!("  {}", hex(&sk.coeffs));
    println!("payload ({} bytes), first 64:", sk.payload.len());
    println!("  {}", hex(&sk.payload[..64]));
    println!(
        "  ↑ payload[j] = XOR_i (coeffs[i] · data_chunk[i][j]) over GF(256). \
         Knowing only this you cannot invert it; uniform distribution over 0..255."
    );
    println!();

    // === Save payload as grayscale PNG so you can see it visually ===
    std::fs::create_dir_all("out").ok();
    let side = side_for(symbol_len);
    let pad = side * side - symbol_len;
    let mut sys_bytes = s0.payload.clone();
    sys_bytes.extend(std::iter::repeat(0).take(pad));
    let mut rlnc_bytes = sk.payload.clone();
    rlnc_bytes.extend(std::iter::repeat(0).take(pad));
    let to_rgb = |b: &[u8]| -> Vec<u8> {
        let mut v = Vec::with_capacity(b.len() * 3);
        for &g in b {
            v.extend_from_slice(&[g, g, g]);
        }
        v
    };
    save_png(
        "out/shard_systematic.png",
        side as u32,
        side as u32,
        &to_rgb(&sys_bytes),
    );
    save_png(
        "out/shard_rlnc.png",
        side as u32,
        side as u32,
        &to_rgb(&rlnc_bytes),
    );

    // === Byte distribution (histogram indicator) ===
    let s0_hist = histogram(&s0.payload);
    let sk_hist = histogram(&sk.payload);
    println!("byte distribution (number of unique values out of 256):");
    println!(
        "  systematic: {} unique, zero share {:.1}%",
        s0_hist.unique,
        s0_hist.zero_fraction * 100.0,
    );
    println!(
        "  RLNC:       {} unique, zero share {:.1}%",
        sk_hist.unique,
        sk_hist.zero_fraction * 100.0,
    );
    println!();
    println!("PNG visualization of the payload as grayscale {}×{}:", side, side);
    println!("  out/shard_systematic.png");
    println!("  out/shard_rlnc.png");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            s.push('\n');
            s.push_str("  ");
        }
        s.push_str(&format!("{:02x} ", b));
    }
    s
}

struct Hist {
    unique: usize,
    zero_fraction: f64,
}

fn histogram(bytes: &[u8]) -> Hist {
    let mut h = [0u32; 256];
    for &b in bytes {
        h[b as usize] += 1;
    }
    Hist {
        unique: h.iter().filter(|&&c| c > 0).count(),
        zero_fraction: h[0] as f64 / bytes.len() as f64,
    }
}

/// Nearest square ≥ n — for the PNG visualization.
fn side_for(n: usize) -> usize {
    let s = (n as f64).sqrt().ceil() as usize;
    s.max(1)
}
