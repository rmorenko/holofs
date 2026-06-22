//! Benchmark harness for the GF/RLNC codec.
//!
//! Measures:
//! - `encode_layer` for various layer sizes
//! - `decode_layer` across three paths:
//!   - **fast**: all K shards are systematic (fresh PUT)
//!   - **partial**: half systematic, half RLNC (a few repairs)
//!   - **full**: only RLNC shards (everything degraded)
//! - `mix_donors` (the hot repair path)
//!
//! No `criterion` or other deps: we measure warm loops with warm-up.
//! Project style: everything by hand.
//!
//! Run: `cargo run --release --bin holofs-bench`.

use std::time::Instant;

use holofs_core::gf::Gf;
use holofs_core::repair::mix_donors;
use holofs_core::rlnc::{decode_layer, encode_layer, Shard};
use holofs_core::rng::Rng;
use holofs_core::K;

fn main() {
    let gf = Gf::new();

    println!("=== holofs bench ===");
    println!("K = {K}, GF(256) scalar (no SIMD)\n");

    // Layer sizes: small (256 bytes) → large (1 MB).
    for &symbol_kb in &[1usize, 4, 16, 64, 256] {
        bench_layer(&gf, symbol_kb);
    }
    bench_mix_donors(&gf);
}

fn bench_layer(gf: &Gf, symbol_kb: usize) {
    let symbol_len = symbol_kb * 1024;
    let data_len = K * symbol_len;
    let mut data = vec![0u8; data_len];
    let mut filler = Rng::new(0xC0FFEE);
    for b in &mut data {
        *b = filler.byte();
    }

    let mut rng = Rng::new(0xBEEF);
    let n = 2 * K + 4;

    // === encode ============================================================
    let iters_encode = pick_iters(symbol_len, 800_000);
    // warm-up
    let (sl, base_shards) = encode_layer(gf, &data, n, &mut rng);
    let t0 = Instant::now();
    for _ in 0..iters_encode {
        let _ = encode_layer(gf, &data, n, &mut rng);
    }
    let enc_ms_per = t0.elapsed().as_secs_f64() * 1000.0 / iters_encode as f64;
    let enc_mb_s = (data_len as f64 / 1e6) / (enc_ms_per / 1000.0);

    // === decode: fast (all K systematic) ===================================
    let refs_fast: Vec<&Shard> = base_shards.iter().take(K).collect();
    let iters_fast = pick_iters(symbol_len, 6_000_000);
    let t0 = Instant::now();
    for _ in 0..iters_fast {
        let r = decode_layer(gf, &refs_fast, sl).unwrap();
        std::hint::black_box(r);
    }
    let fast_ms_per = t0.elapsed().as_secs_f64() * 1000.0 / iters_fast as f64;
    let fast_mb_s = (data_len as f64 / 1e6) / (fast_ms_per / 1000.0);

    // === decode: partial (K/2 systematic + K/2 RLNC) =======================
    let mut refs_partial: Vec<&Shard> = base_shards.iter().take(K / 2).collect();
    refs_partial.extend(base_shards.iter().skip(K).take(K / 2));
    let iters_part = pick_iters(symbol_len, 800_000);
    let t0 = Instant::now();
    for _ in 0..iters_part {
        let r = decode_layer(gf, &refs_partial, sl).unwrap();
        std::hint::black_box(r);
    }
    let part_ms_per = t0.elapsed().as_secs_f64() * 1000.0 / iters_part as f64;
    let part_mb_s = (data_len as f64 / 1e6) / (part_ms_per / 1000.0);

    // === decode: full (RLNC only) ==========================================
    let refs_full: Vec<&Shard> = base_shards.iter().skip(K).take(K).collect();
    let iters_full = pick_iters(symbol_len, 400_000);
    let t0 = Instant::now();
    for _ in 0..iters_full {
        let r = decode_layer(gf, &refs_full, sl).unwrap();
        std::hint::black_box(r);
    }
    let full_ms_per = t0.elapsed().as_secs_f64() * 1000.0 / iters_full as f64;
    let full_mb_s = (data_len as f64 / 1e6) / (full_ms_per / 1000.0);

    let speedup_fast = full_ms_per / fast_ms_per;
    let speedup_part = full_ms_per / part_ms_per;

    println!(
        "symbol={symbol_kb} KB  layer={:>5} KB  | encode {enc_ms_per:>6.2} ms ({enc_mb_s:>6.1} MB/s)",
        data_len / 1024
    );
    println!(
        "                              decode fast    {fast_ms_per:>6.2} ms ({fast_mb_s:>6.1} MB/s)  ×{speedup_fast:>4.1} vs full"
    );
    println!(
        "                              decode partial {part_ms_per:>6.2} ms ({part_mb_s:>6.1} MB/s)  ×{speedup_part:>4.1} vs full"
    );
    println!(
        "                              decode full    {full_ms_per:>6.2} ms ({full_mb_s:>6.1} MB/s)\n"
    );
}

fn bench_mix_donors(gf: &Gf) {
    // Repair: simulate a layer with symbol_len=16 KB, donors = K shards.
    let symbol_len = 16 * 1024;
    let data = {
        let mut v = vec![0u8; K * symbol_len];
        let mut r = Rng::new(11);
        for b in &mut v {
            *b = r.byte();
        }
        v
    };
    let mut rng = Rng::new(0x77);
    let (_sl, donors) = encode_layer(gf, &data, K, &mut rng);
    let need = 4; // generate 4 fresh shards for a new node
    let iters = 200_000;

    let t0 = Instant::now();
    for _ in 0..iters {
        let r = mix_donors(gf, &donors, need, &mut rng);
        std::hint::black_box(r);
    }
    let ms_per = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    println!("=== mix_donors ===");
    println!(
        "K donors → {need} fresh shards  symbol_len={} KB:  {ms_per:>6.3} ms/itr",
        symbol_len / 1024
    );
}

/// Pick iteration count so the total work is ~ N bytes.
fn pick_iters(symbol_len: usize, target: usize) -> u32 {
    let work_per_iter = (K * symbol_len).max(1);
    ((target * 1024 / work_per_iter).max(50) as u32).min(50_000)
}
