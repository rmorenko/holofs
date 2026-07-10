//! Random Linear Network Coding over GF(256), systematic mode.
//!
//! Shard = (K-dim coefficient vector over GF(256)) + (payload).
//!
//! Encoding emits two kinds of shard:
//! - **Systematic** (first `min(K, n)` shards): `coeffs[i] = e_i` (zeros plus a
//!   single one), `payload` = i-th raw data chunk. Decoding from such a shard
//!   is a plain copy.
//! - **RLNC** (remaining `n - K` shards): ordinary random linear combinations
//!   of the K raw symbols over GF(256), as before.
//!
//! Decoding has three paths:
//! - **Fast**: all K raw chunks arrived as systematic → concatenate
//!   (O(K·sl), no GF multiplies, no Gauss).
//! - **Partial**: some chunks known directly, the rest are recovered via
//!   Gauss-Jordan over the unknown columns only (`n_unknown` × `n_unknown`).
//! - **Full**: zero systematic shards → the standard K×K Gauss path (the
//!   pre-Stage-6 fallback).
//!
//! Note: "repair does not produce systematic shards" — `mix_donors` always
//! yields RLNC shards, so after a series of repairs some original systematic
//! shards are replaced by random ones, and decode slides gracefully from fast
//! to partial to full.

use crate::gf::Gf;
use crate::rng::Rng;
use crate::K;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shard {
    pub coeffs: Vec<u8>,
    pub payload: Vec<u8>,
}

/// If `coeffs` represents basis vector `e_i` (one 1, rest zeros), return `i`.
/// Otherwise return `None`.
fn identity_index(coeffs: &[u8]) -> Option<usize> {
    let mut idx = None;
    for (i, &c) in coeffs.iter().enumerate() {
        if c == 1 {
            if idx.is_some() {
                return None;
            }
            idx = Some(i);
        } else if c != 0 {
            return None;
        }
    }
    idx
}

/// Same as `encode_layer` but with an arbitrary `k` (instead of the constant
/// `crate::K`). Used by the escrow mode (`holofs-key-escrow`), where the user
/// chooses the threshold (K) and the total number of shares (N).
pub fn encode_layer_with_k(
    gf: &Gf,
    data: &[u8],
    k: usize,
    n: usize,
    rng: &mut Rng,
) -> (usize, Vec<Shard>) {
    let symbol_len = (data.len() + k - 1) / k;
    let mut padded = data.to_vec();
    padded.resize(k * symbol_len, 0);
    let mut shards = Vec::with_capacity(n);

    let n_systematic = n.min(k);
    for i in 0..n_systematic {
        let mut coeffs = vec![0u8; k];
        coeffs[i] = 1;
        let payload = padded[i * symbol_len..(i + 1) * symbol_len].to_vec();
        shards.push(Shard { coeffs, payload });
    }
    for _ in n_systematic..n {
        let mut coeffs = vec![0u8; k];
        for c in coeffs.iter_mut() {
            *c = rng.byte();
        }
        if coeffs.iter().filter(|&&b| b != 0).count() < 2 {
            coeffs[0] ^= 0x80;
            if k > 1 {
                coeffs[1] ^= 0x40;
            }
        }
        let mut payload = vec![0u8; symbol_len];
        for i in 0..k {
            let ci = coeffs[i];
            if ci == 0 {
                continue;
            }
            let src = &padded[i * symbol_len..(i + 1) * symbol_len];
            for j in 0..symbol_len {
                payload[j] ^= gf.mul(ci, src[j]);
            }
        }
        shards.push(Shard { coeffs, payload });
    }
    (symbol_len, shards)
}

/// Non-systematic variant of [`encode_layer_with_k`] — every one of `n`
/// shards is a fresh random linear combination of all `k` source chunks.
/// The systematic path (`e_i` shards carrying a raw chunk) is exactly
/// what turns escrow-style secret sharing from information-theoretic
/// into "one share reveals 1/K of the plaintext", so the escrow crate
/// uses this path instead.
///
/// Non-zero row guarantee: if the RNG happens to hand back an all-zero
/// coefficient vector (probability `2^-8k`) we replace it with a
/// deterministic non-zero pattern — a zero row would silently drop the
/// share from the recovery set.
pub fn encode_layer_with_k_random(
    gf: &Gf,
    data: &[u8],
    k: usize,
    n: usize,
    rng: &mut Rng,
) -> (usize, Vec<Shard>) {
    let symbol_len = data.len().div_ceil(k);
    let mut padded = data.to_vec();
    padded.resize(k * symbol_len, 0);
    let mut shards = Vec::with_capacity(n);
    for _ in 0..n {
        let mut coeffs = vec![0u8; k];
        for c in coeffs.iter_mut() {
            *c = rng.byte();
        }
        // Guarantee at least two non-zero coefficients: a single
        // non-zero coefficient means the share is a scalar multiple
        // of one raw chunk and reveals it modulo a public scalar,
        // defeating the point of dropping the systematic path.
        if coeffs.iter().filter(|&&b| b != 0).count() < 2 {
            coeffs[0] ^= 0x80;
            if k > 1 {
                coeffs[1] ^= 0x40;
            }
        }
        let mut payload = vec![0u8; symbol_len];
        for i in 0..k {
            let ci = coeffs[i];
            if ci == 0 {
                continue;
            }
            let src = &padded[i * symbol_len..(i + 1) * symbol_len];
            for j in 0..symbol_len {
                payload[j] ^= gf.mul(ci, src[j]);
            }
        }
        shards.push(Shard { coeffs, payload });
    }
    (symbol_len, shards)
}

/// Same as `decode_layer` but with an arbitrary `k`. Full Gauss-Jordan;
/// supports systematic+RLNC.
pub fn decode_layer_with_k(
    gf: &Gf,
    shards: &[&Shard],
    k: usize,
    symbol_len: usize,
) -> Option<Vec<u8>> {
    // Drop any shard whose length doesn't match the declared
    // (k, symbol_len). Before this filter, a shard with short
    // `coeffs` or `payload` (crafted, corrupted, or arriving from
    // an older schema) would panic on `coeffs[i]` / `payload[j]`
    // deep in the solver — with attacker-supplied lengths reachable
    // through `escrow_recover` on the web boundary, that's a DoS
    // vector.
    let shards: Vec<&Shard> = shards
        .iter()
        .copied()
        .filter(|s| s.coeffs.len() == k && s.payload.len() == symbol_len)
        .collect();
    if shards.len() < k {
        return None;
    }
    let mut known: Vec<Option<&[u8]>> = vec![None; k];
    let mut rlnc: Vec<&Shard> = Vec::new();
    for &s in &shards {
        let mut idx = None;
        let mut bad = false;
        for (i, &c) in s.coeffs.iter().enumerate() {
            if c == 1 {
                if idx.is_some() {
                    bad = true;
                    break;
                }
                idx = Some(i);
            } else if c != 0 {
                bad = true;
                break;
            }
        }
        match (idx, bad) {
            (Some(i), false) if i < k && known[i].is_none() => {
                known[i] = Some(&s.payload);
            }
            _ => rlnc.push(s),
        }
    }
    let n_known = known.iter().filter(|c| c.is_some()).count();
    if n_known == k {
        let mut out = Vec::with_capacity(k * symbol_len);
        for c in &known {
            out.extend_from_slice(c.unwrap());
        }
        return Some(out);
    }
    let n_unknown = k - n_known;
    if rlnc.len() < n_unknown {
        return None;
    }
    let unknown_positions: Vec<usize> = (0..k).filter(|i| known[*i].is_none()).collect();
    let rl = n_unknown + symbol_len;
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(rlnc.len());
    for s in &rlnc {
        let mut row = vec![0u8; rl];
        for (col, &pos) in unknown_positions.iter().enumerate() {
            row[col] = s.coeffs[pos];
        }
        for j in 0..symbol_len {
            let mut byte = s.payload[j];
            for i in 0..k {
                if let Some(chunk) = known[i] {
                    let c = s.coeffs[i];
                    if c != 0 {
                        byte ^= gf.mul(c, chunk[j]);
                    }
                }
            }
            row[n_unknown + j] = byte;
        }
        rows.push(row);
    }
    let mut pr = 0usize;
    for col in 0..n_unknown {
        let mut sel = None;
        for r in pr..rows.len() {
            if rows[r][col] != 0 {
                sel = Some(r);
                break;
            }
        }
        let sel = sel?;
        rows.swap(pr, sel);
        let inv = gf.inv(rows[pr][col]);
        for x in 0..rl {
            rows[pr][x] = gf.mul(rows[pr][x], inv);
        }
        for r in 0..rows.len() {
            if r == pr {
                continue;
            }
            let f = rows[r][col];
            if f == 0 {
                continue;
            }
            for x in 0..rl {
                let t = gf.mul(f, rows[pr][x]);
                rows[r][x] ^= t;
            }
        }
        pr += 1;
    }
    let mut out = Vec::with_capacity(k * symbol_len);
    for i in 0..k {
        if let Some(chunk) = known[i] {
            out.extend_from_slice(chunk);
        } else {
            let row_idx = unknown_positions.iter().position(|&p| p == i).unwrap();
            out.extend_from_slice(&rows[row_idx][n_unknown..rl]);
        }
    }
    Some(out)
}

/// Encode a layer into `n` shards: the first `min(n, K)` are systematic
/// (separate chunks of the raw data); the rest are random linear combinations.
/// Returns (symbol length in bytes, shards).
pub fn encode_layer(gf: &Gf, data: &[u8], n: usize, rng: &mut Rng) -> (usize, Vec<Shard>) {
    let symbol_len = (data.len() + K - 1) / K;
    let mut padded = data.to_vec();
    padded.resize(K * symbol_len, 0);
    let mut shards = Vec::with_capacity(n);

    // === Systematic shards (first min(n, K)) ==============================
    let n_systematic = n.min(K);
    for i in 0..n_systematic {
        let mut coeffs = vec![0u8; K];
        coeffs[i] = 1;
        let payload = padded[i * symbol_len..(i + 1) * symbol_len].to_vec();
        shards.push(Shard { coeffs, payload });
    }

    // === RLNC shards (remaining n - K, for redundancy) ====================
    for _ in n_systematic..n {
        let mut coeffs = vec![0u8; K];
        for c in coeffs.iter_mut() {
            *c = rng.byte();
        }
        // Guard against degenerate cases: the zero vector and random matches
        // with a basis vector. Force at least two non-zero positions — that
        // guarantees the shard is not a "duplicate" of any systematic one.
        if coeffs.iter().filter(|&&b| b != 0).count() < 2 {
            coeffs[0] ^= 0x80;
            coeffs[1] ^= 0x40;
        }
        let mut payload = vec![0u8; symbol_len];
        for i in 0..K {
            let ci = coeffs[i];
            if ci == 0 {
                continue;
            }
            let src = &padded[i * symbol_len..(i + 1) * symbol_len];
            for j in 0..symbol_len {
                payload[j] ^= gf.mul(ci, src[j]);
            }
        }
        shards.push(Shard { coeffs, payload });
    }
    (symbol_len, shards)
}

/// Decode a layer **with holes**: returns a `Vec<Option<Vec<u8>>>` of length K
/// where `Some(chunk_i)` is the recovered i-th source chunk, and `None`
/// indicates that there were not enough shards to recover that chunk. This is
/// the text path: shard loss manifests as **positional holes**, not
/// "all or nothing".
///
/// Algorithm:
/// 1. First gather what is known directly: systematic shards give `chunk_i`
///    for free (`coeffs = e_i`, `payload` = chunk).
/// 2. For the remaining positions try to solve the reduced
///    `n_rlnc × n_unknown` Gauss system. Pivot columns become `Some(...)`;
///    non-pivot ones stay `None`.
///
/// If fewer than K shards are available, return an array of length K with
/// what we managed to recover. This differs from `decode_layer`, which
/// returns `None` in that case.
pub fn decode_layer_with_holes(
    gf: &Gf,
    shards: &[&Shard],
    symbol_len: usize,
) -> Vec<Option<Vec<u8>>> {
    // Same crash-safety filter as `decode_layer_with_k` — reject
    // shards whose (coeffs, payload) lengths don't match the (K,
    // symbol_len) contract. Silent skip is safer than an index
    // panic when the caller cannot guarantee shard provenance.
    let shards: Vec<&Shard> = shards
        .iter()
        .copied()
        .filter(|s| s.coeffs.len() == K && s.payload.len() == symbol_len)
        .collect();
    let mut known: Vec<Option<&[u8]>> = vec![None; K];
    let mut rlnc: Vec<&Shard> = Vec::new();
    for &s in &shards {
        match identity_index(&s.coeffs) {
            Some(i) if known[i].is_none() => {
                known[i] = Some(&s.payload);
            }
            Some(_) => {} // duplicate — ignore
            None => rlnc.push(s),
        }
    }

    // Base result: systematic chunks are filled in immediately.
    let mut result: Vec<Option<Vec<u8>>> = known.iter().map(|c| c.map(|b| b.to_vec())).collect();

    let unknown_positions: Vec<usize> = (0..K).filter(|i| known[*i].is_none()).collect();
    let n_unknown = unknown_positions.len();
    if n_unknown == 0 || rlnc.is_empty() {
        return result;
    }

    // Build the reduced system: each row = an RLNC shard with the known
    // contributions subtracted. n_unknown matrix columns + symbol_len payload bytes.
    let rl = n_unknown + symbol_len;
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(rlnc.len());
    for s in &rlnc {
        let mut row = vec![0u8; rl];
        for (col, &pos) in unknown_positions.iter().enumerate() {
            row[col] = s.coeffs[pos];
        }
        for j in 0..symbol_len {
            let mut byte = s.payload[j];
            for i in 0..K {
                if let Some(chunk) = known[i] {
                    let c = s.coeffs[i];
                    if c != 0 {
                        byte ^= gf.mul(c, chunk[j]);
                    }
                }
            }
            row[n_unknown + j] = byte;
        }
        rows.push(row);
    }

    // Gauss-Jordan. Track which columns ended up being pivots — only those
    // unknown positions are actually recovered.
    let mut pivot_row_for_col: Vec<Option<usize>> = vec![None; n_unknown];
    let mut pr = 0usize;
    for col in 0..n_unknown {
        let mut sel = None;
        for r in pr..rows.len() {
            if rows[r][col] != 0 {
                sel = Some(r);
                break;
            }
        }
        let Some(sel) = sel else { continue };
        rows.swap(pr, sel);
        let inv = gf.inv(rows[pr][col]);
        for x in 0..rl {
            rows[pr][x] = gf.mul(rows[pr][x], inv);
        }
        for r in 0..rows.len() {
            if r == pr {
                continue;
            }
            let f = rows[r][col];
            if f == 0 {
                continue;
            }
            for x in 0..rl {
                let t = gf.mul(f, rows[pr][x]);
                rows[r][x] ^= t;
            }
        }
        pivot_row_for_col[col] = Some(pr);
        pr += 1;
    }

    // Recover the chunks for which a pivot was found.
    //
    // IMPORTANT: the pivot row yields a clean chunk[i] = payload_row only if
    // ALL other columns in that row are zero. If the row still has a nonzero
    // coefficient on some non-pivot column, the value is "dirty" (contaminated
    // by an unknown chunk) — mark such a chunk as unrecoverable so we do not
    // hand garbage to the client.
    for (col, &pos) in unknown_positions.iter().enumerate() {
        let Some(row_idx) = pivot_row_for_col[col] else {
            continue;
        };
        let row = &rows[row_idx];
        // Check: only the col-th column is nonzero in this row among n_unknown.
        let clean = (0..n_unknown).all(|c| c == col || row[c] == 0);
        if clean {
            result[pos] = Some(row[n_unknown..rl].to_vec());
        }
    }
    result
}

/// Decode a layer. Requires ≥ K shards in total. Uses systematic shards as
/// "free rows", saving Gauss work.
pub fn decode_layer(gf: &Gf, shards: &[&Shard], symbol_len: usize) -> Option<Vec<u8>> {
    // Crash-safety filter — see `decode_layer_with_k`.
    let shards: Vec<&Shard> = shards
        .iter()
        .copied()
        .filter(|s| s.coeffs.len() == K && s.payload.len() == symbol_len)
        .collect();
    if shards.len() < K {
        return None;
    }

    let mut known: Vec<Option<&[u8]>> = vec![None; K];
    let mut rlnc: Vec<&Shard> = Vec::new();
    for &s in &shards {
        match identity_index(&s.coeffs) {
            Some(i) if known[i].is_none() => {
                known[i] = Some(&s.payload);
            }
            Some(_) => {
                // Duplicate of an already-known chunk — ignore.
            }
            None => rlnc.push(s),
        }
    }
    let n_known = known.iter().filter(|c| c.is_some()).count();

    // === Fast path: all K raw chunks arrived as systematic ===============
    if n_known == K {
        let mut out = Vec::with_capacity(K * symbol_len);
        for c in &known {
            out.extend_from_slice(c.unwrap());
        }
        return Some(out);
    }

    let n_unknown = K - n_known;
    if rlnc.len() < n_unknown {
        // Not enough RLNC shards to recover the remaining chunks.
        return None;
    }

    // the reduced system n_unknown × n_unknown.
    let unknown_positions: Vec<usize> = (0..K).filter(|i| known[*i].is_none()).collect();
    let rl = n_unknown + symbol_len;
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(rlnc.len());
    for s in &rlnc {
        let mut row = vec![0u8; rl];
        for (col, &pos) in unknown_positions.iter().enumerate() {
            row[col] = s.coeffs[pos];
        }
        // payload XOR (Σ coeffs[i] · known[i]) for every known i.
        for j in 0..symbol_len {
            let mut byte = s.payload[j];
            for i in 0..K {
                if let Some(chunk) = known[i] {
                    let c = s.coeffs[i];
                    if c != 0 {
                        byte ^= gf.mul(c, chunk[j]);
                    }
                }
            }
            row[n_unknown + j] = byte;
        }
        rows.push(row);
    }

    let mut pr = 0usize;
    for col in 0..n_unknown {
        let mut sel = None;
        for r in pr..rows.len() {
            if rows[r][col] != 0 {
                sel = Some(r);
                break;
            }
        }
        let sel = sel?;
        rows.swap(pr, sel);
        let inv = gf.inv(rows[pr][col]);
        for x in 0..rl {
            rows[pr][x] = gf.mul(rows[pr][x], inv);
        }
        for r in 0..rows.len() {
            if r == pr {
                continue;
            }
            let f = rows[r][col];
            if f == 0 {
                continue;
            }
            for x in 0..rl {
                let t = gf.mul(f, rows[pr][x]);
                rows[r][x] ^= t;
            }
        }
        pr += 1;
    }

    let mut out = Vec::with_capacity(K * symbol_len);
    for i in 0..K {
        if let Some(chunk) = known[i] {
            out.extend_from_slice(chunk);
        } else {
            let row_idx = unknown_positions.iter().position(|&p| p == i).unwrap();
            out.extend_from_slice(&rows[row_idx][n_unknown..rl]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn truncate_to(data: &[u8], decoded: Vec<u8>) -> Vec<u8> {
        decoded.into_iter().take(data.len()).collect()
    }

    #[test]
    fn roundtrip_exact_k_shards() {
        let gf = Gf::new();
        let mut rng = Rng::new(42);
        let data: Vec<u8> = (0..K * 100).map(|i| (i as u8).wrapping_mul(7)).collect();
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        let refs: Vec<&Shard> = shards.iter().collect();
        let decoded = decode_layer(&gf, &refs, sl).expect("decode must succeed");
        assert_eq!(truncate_to(&data, decoded), data);
    }

    #[test]
    fn k_plus_buffer_window_decodes() {
        // Systematic RLNC is not MDS: a window of exactly K shards can be
        // linearly dependent (e.g. e_1..e_15 + one RLNC shard with `coeffs[0] = 0`
        // does not cover position 0). A K+4 window is statistically enough for
        // any combination.
        let gf = Gf::new();
        let mut rng = Rng::new(7);
        let data: Vec<u8> = (0..K * 50).map(|i| (i * 13 + 5) as u8).collect();
        let n = 2 * K + 4;
        let (sl, shards) = encode_layer(&gf, &data, n, &mut rng);

        let window = K + 4;
        for skip in [0usize, 1, 5, K, n - window] {
            let refs: Vec<&Shard> = shards.iter().skip(skip).take(window).collect();
            assert_eq!(refs.len(), window);
            let decoded = decode_layer(&gf, &refs, sl).expect("K+4 window decodes");
            assert_eq!(truncate_to(&data, decoded), data, "skip={skip}");
        }
    }

    #[test]
    fn fewer_than_k_returns_none() {
        let gf = Gf::new();
        let mut rng = Rng::new(1);
        let data = vec![1u8; K * 8];
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        let refs: Vec<&Shard> = shards.iter().take(K - 1).collect();
        assert!(decode_layer(&gf, &refs, sl).is_none());
    }

    #[test]
    fn handles_minimal_data() {
        // Exactly K bytes — symbol_len = 1.
        let gf = Gf::new();
        let mut rng = Rng::new(99);
        let data: Vec<u8> = (0..K).map(|i| i as u8).collect();
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        assert_eq!(sl, 1);
        let refs: Vec<&Shard> = shards.iter().collect();
        let decoded = decode_layer(&gf, &refs, sl).unwrap();
        assert_eq!(&decoded[..K], &data[..]);
    }

    #[test]
    fn padding_does_not_corrupt_when_data_not_multiple_of_k() {
        let gf = Gf::new();
        let mut rng = Rng::new(123);
        // 33 bytes, K=16 → symbol_len = ceil(33/16) = 3; padded = 48.
        let data: Vec<u8> = (0..33).map(|i| (i as u8) ^ 0xA5).collect();
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        let refs: Vec<&Shard> = shards.iter().collect();
        let decoded = decode_layer(&gf, &refs, sl).unwrap();
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    #[test]
    fn custom_k_3_of_5_roundtrip() {
        // Escrow-style: 3 of 5, K=3.
        let gf = Gf::new();
        let mut rng = Rng::new(1);
        let data: Vec<u8> = (0..300).map(|i| i as u8).collect();
        let (sl, shards) = encode_layer_with_k(&gf, &data, 3, 5, &mut rng);
        assert_eq!(shards.len(), 5);
        // Any 3 of 5 → recovery.
        let combos: Vec<Vec<usize>> =
            vec![vec![0, 1, 2], vec![0, 1, 4], vec![1, 3, 4], vec![2, 3, 4]];
        for combo in combos {
            let refs: Vec<&Shard> = combo.iter().map(|&i| &shards[i]).collect();
            let decoded = decode_layer_with_k(&gf, &refs, 3, sl).unwrap();
            assert_eq!(&decoded[..data.len()], &data[..]);
        }
        // 2 shards — not enough.
        let refs: Vec<&Shard> = vec![&shards[0], &shards[1]];
        assert!(decode_layer_with_k(&gf, &refs, 3, sl).is_none());
    }

    #[test]
    fn custom_k_2_of_3_minimal() {
        let gf = Gf::new();
        let mut rng = Rng::new(2);
        let data = b"hello world this is a secret message".to_vec();
        let (sl, shards) = encode_layer_with_k(&gf, &data, 2, 3, &mut rng);
        assert_eq!(shards.len(), 3);
        let refs: Vec<&Shard> = vec![&shards[1], &shards[2]];
        let decoded = decode_layer_with_k(&gf, &refs, 2, sl).unwrap();
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    #[test]
    fn duplicate_shards_fail_to_decode() {
        // K copies of one shard are linearly dependent, rank 1; decode must
        // return None. Under systematic encode shard 0 = e_0; K copies of one
        // e_0 cover only position 0, the other K-1 positions cannot be recovered.
        let gf = Gf::new();
        let mut rng = Rng::new(5);
        let data = vec![9u8; K * 4];
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        let refs: Vec<&Shard> = (0..K).map(|_| &shards[0]).collect();
        assert!(decode_layer(&gf, &refs, sl).is_none());
    }

    #[test]
    fn first_k_shards_are_systematic() {
        let gf = Gf::new();
        let mut rng = Rng::new(0xC1);
        let data: Vec<u8> = (0..K * 7).map(|i| i as u8).collect();
        let (sl, shards) = encode_layer(&gf, &data, K + 4, &mut rng);
        for (i, s) in shards.iter().take(K).enumerate() {
            assert_eq!(
                identity_index(&s.coeffs),
                Some(i),
                "shard {i} must be systematic e_{i}"
            );
            assert_eq!(s.payload, &data[i * sl..(i + 1) * sl]);
        }
        // Shards K..K+4 are RLNC, not identity.
        for s in shards.iter().skip(K) {
            assert!(identity_index(&s.coeffs).is_none());
            assert!(s.coeffs.iter().filter(|&&b| b != 0).count() >= 2);
        }
    }

    #[test]
    fn fast_path_decodes_all_systematic() {
        // All K systematic shards on input → decode is plain concatenation.
        let gf = Gf::new();
        let mut rng = Rng::new(7);
        let data: Vec<u8> = (0..K * 20).map(|i| (i as u8) ^ 0x5A).collect();
        let (sl, shards) = encode_layer(&gf, &data, K + 6, &mut rng);
        let refs: Vec<&Shard> = shards.iter().take(K).collect();
        let decoded = decode_layer(&gf, &refs, sl).unwrap();
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    #[test]
    fn partial_path_decodes_mixed_systematic_and_rlnc() {
        // Half of the systematic shards are alive; the rest is RLNC.
        let gf = Gf::new();
        let mut rng = Rng::new(0x42);
        let data: Vec<u8> = (0..K * 15).map(|i| ((i * 17) ^ 0x33) as u8).collect();
        let n = 2 * K;
        let (sl, shards) = encode_layer(&gf, &data, n, &mut rng);

        // Take systematic shards 0..K/2 plus RLNC shards K..K + K/2.
        let mut refs: Vec<&Shard> = shards.iter().take(K / 2).collect();
        refs.extend(shards.iter().skip(K).take(K / 2));
        assert_eq!(refs.len(), K);
        let decoded = decode_layer(&gf, &refs, sl).unwrap();
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    #[test]
    fn decode_with_holes_recovers_all_when_enough_shards() {
        // All K systematic + extra RLNC → every chunk is recovered.
        let gf = Gf::new();
        let mut rng = Rng::new(1);
        let data: Vec<u8> = (0..K * 8).map(|i| i as u8).collect();
        let (sl, shards) = encode_layer(&gf, &data, 2 * K, &mut rng);
        let refs: Vec<&Shard> = shards.iter().take(K).collect();
        let chunks = decode_layer_with_holes(&gf, &refs, sl);
        assert_eq!(chunks.len(), K);
        for c in &chunks {
            assert!(c.is_some(), "all K systematic → zero holes");
        }
    }

    #[test]
    fn decode_with_holes_keeps_holes_for_missing_chunks() {
        // Systematic shards 0..K-3 live, the rest are gone. RLNC=0.
        // Must return K chunks: 13 alive + 3 holes.
        let gf = Gf::new();
        let mut rng = Rng::new(2);
        let data: Vec<u8> = (0..K * 8).map(|i| (i * 31) as u8).collect();
        let (sl, shards) = encode_layer(&gf, &data, K, &mut rng);
        let n_alive = K - 3;
        let refs: Vec<&Shard> = shards.iter().take(n_alive).collect();
        let chunks = decode_layer_with_holes(&gf, &refs, sl);
        assert_eq!(chunks.len(), K);
        let holes = chunks.iter().filter(|c| c.is_none()).count();
        assert_eq!(holes, 3, "must be exactly 3 holes");
        // Live chunks must match the source.
        for i in 0..n_alive {
            assert_eq!(
                chunks[i].as_ref().map(|c| &c[..]),
                Some(&data[i * sl..(i + 1) * sl])
            );
        }
    }

    #[test]
    fn decode_with_holes_partial_systematic_plus_some_rlnc() {
        // K=16, we have 10 systematic + 3 RLNC → we can recover
        // ≥ 10 (exact) + up to 3 (if the RLNC shards are independent across
        // positions) = up to 13. We guarantee unknown.len() = 6 and RLNC
        // covers at most 3.
        let gf = Gf::new();
        let mut rng = Rng::new(3);
        let data: Vec<u8> = (0..K * 4).map(|i| (i ^ 0x55) as u8).collect();
        let (sl, shards) = encode_layer(&gf, &data, 2 * K, &mut rng);
        // Take the first 10 systematic + 3 RLNC.
        let mut refs: Vec<&Shard> = shards.iter().take(10).collect();
        refs.extend(shards.iter().skip(K).take(3));
        let chunks = decode_layer_with_holes(&gf, &refs, sl);
        let recovered = chunks.iter().filter(|c| c.is_some()).count();
        assert!(
            recovered >= 10 && recovered <= 13,
            "expected 10..=13 recovered, got {recovered}"
        );
        // The first 10 chunks are guaranteed alive (systematic).
        for i in 0..10 {
            assert!(chunks[i].is_some(), "systematic {i} must be alive");
            assert_eq!(
                chunks[i].as_ref().map(|c| &c[..]),
                Some(&data[i * sl..(i + 1) * sl])
            );
        }
    }

    #[test]
    fn full_path_decodes_only_rlnc() {
        // Zero systematic shards — fallback to the full Gauss path.
        let gf = Gf::new();
        let mut rng = Rng::new(0x99);
        let data: Vec<u8> = (0..K * 12).map(|i| (i as u8).wrapping_mul(31)).collect();
        let n = 3 * K;
        let (sl, shards) = encode_layer(&gf, &data, n, &mut rng);

        // RLNC shards start at index K.
        let refs: Vec<&Shard> = shards.iter().skip(K).take(K).collect();
        let decoded = decode_layer(&gf, &refs, sl).unwrap();
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    /// B5 regression: a shard whose lengths disagree with the (K,
    /// symbol_len) contract must be filtered out — the decoder used
    /// to index straight into `coeffs[i]` / `payload[j]` and panic.
    #[test]
    fn decode_layer_rejects_malformed_shard_lengths() {
        let gf = Gf::new();
        let mut rng = Rng::new(31337);
        let data: Vec<u8> = (0..K * 4).map(|i| i as u8).collect();
        let (sl, mut shards) = encode_layer(&gf, &data, K + 8, &mut rng);
        // Corrupt one shard's coeffs length (short).
        shards[3].coeffs.truncate(K - 1);
        // Corrupt another's payload length (extra byte).
        shards[5].payload.push(0);
        let refs: Vec<&Shard> = shards.iter().collect();
        // With 2 shards dropped we still have K + 6, so decode must
        // succeed — importantly, without panicking.
        let decoded = decode_layer(&gf, &refs, sl).expect("decode succeeds with survivors");
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    /// B5 regression, harsher path: enough malformed shards that no
    /// valid K remain. Decoder must return `None`, not crash.
    #[test]
    fn decode_layer_returns_none_when_all_shards_malformed() {
        let gf = Gf::new();
        let mut rng = Rng::new(4);
        let data: Vec<u8> = (0..K * 2).map(|i| i as u8).collect();
        let (sl, mut shards) = encode_layer(&gf, &data, K, &mut rng);
        for s in shards.iter_mut() {
            s.coeffs.pop();
        }
        let refs: Vec<&Shard> = shards.iter().collect();
        assert!(decode_layer(&gf, &refs, sl).is_none());
    }
}
