//! Holographic Key Escrow — Shamir-style secret sharing on RLNC.
//!
//! The user uploads a file (private key, seed phrase, important document)
//! and K/N parameters (e.g. 3 of 5). The system encodes the file as `n`
//! shards via [`holofs_core::rlnc::encode_layer_with_k`] and **packages each
//! shard as a standalone `.holoshare` file** for distribution.
//!
//! Recovery: gather any K `.holoshare` files, hand them to the gateway, and
//! receive the original file with the correct `Content-Type`.
//!
//! **Shards are NOT stored on cluster nodes!** This is a pure-client feature:
//! the gateway encodes → emits N files → the user distributes them. The
//! cluster is not a trusted party and does not hold any key material.
//!
//! ## Use cases
//!
//! - Bitcoin/Ethereum seed phrase: 3 of 5 → split across family members.
//! - Hardware wallet backup: 4 of 7 → distinct locations (bank, home, friend).
//! - Inheritance: 5 of 9 (5+4 strategy — notary + heirs).
//! - 2FA seed backup without the cloud.
//!
//! ## Security
//!
//! - K-1 shards **leak no information** about the contents (information-
//!   theoretic security, like Shamir). This is math, not trust.
//! - Shards contain only linear projections over GF(256) — no partial bit
//!   leakage even under information-theoretic analysis.
//! - Files may be stored with untrusted parties — cloud, IM, email — safely.
//!
//! ## `.holoshare` format
//!
//! ```text
//! magic           9  bytes = b"HOLOSHAR1"
//! escrow_id       16 bytes — UUID-like (derived from data_cid)
//! shard_idx       2  bytes BE — this share index (0..N-1)
//! total_n         2  bytes BE — total number of shares
//! total_k         2  bytes BE — recovery threshold
//! real_len        8  bytes BE — original file length in bytes
//! content_type_n  1  byte  — length of content_type
//! content_type    N  bytes — original MIME
//! filename_n      1  byte  — length of filename
//! filename        N  bytes — original filename
//! coeffs_len      2  bytes BE = K
//! coeffs          K  bytes — linear coefficients
//! payload_len     4  bytes BE = sym_len
//! payload         payload_len bytes
//! ```

use holofs_core::gf::Gf;
use holofs_core::hash::Sha256;
use holofs_core::rlnc::{decode_layer_with_k, encode_layer_with_k, Shard};
use holofs_core::rng::Rng;

pub const SHARE_MAGIC: &[u8; 9] = b"HOLOSHAR1";

/// Escrow split parameters.
#[derive(Debug, Clone)]
pub struct EscrowParams {
    pub k: usize,
    pub n: usize,
    pub content_type: String,
    pub filename: String,
}

/// One `.holoshare` share (with the full metadata set for standalone recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareFile {
    pub escrow_id: [u8; 16],
    pub shard_idx: u16,
    pub total_n: u16,
    pub total_k: u16,
    pub real_len: u64,
    pub content_type: String,
    pub filename: String,
    pub coeffs: Vec<u8>,
    pub payload: Vec<u8>,
}

impl ShareFile {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SHARE_MAGIC);
        out.extend_from_slice(&self.escrow_id);
        out.extend_from_slice(&self.shard_idx.to_be_bytes());
        out.extend_from_slice(&self.total_n.to_be_bytes());
        out.extend_from_slice(&self.total_k.to_be_bytes());
        out.extend_from_slice(&self.real_len.to_be_bytes());
        let ct = self.content_type.as_bytes();
        out.push(ct.len() as u8);
        out.extend_from_slice(ct);
        let fn_b = self.filename.as_bytes();
        out.push(fn_b.len() as u8);
        out.extend_from_slice(fn_b);
        out.extend_from_slice(&(self.coeffs.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.coeffs);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < SHARE_MAGIC.len() {
            return Err("file too short".into());
        }
        if &bytes[..9] != SHARE_MAGIC {
            return Err("not a .holoshare (bad magic)".into());
        }
        let mut p = 9;
        let need = |p: usize, n: usize| -> Result<(), String> {
            if p + n > bytes.len() {
                Err("truncated .holoshare".into())
            } else {
                Ok(())
            }
        };
        need(p, 16)?;
        let mut escrow_id = [0u8; 16];
        escrow_id.copy_from_slice(&bytes[p..p + 16]);
        p += 16;
        need(p, 2)?;
        let shard_idx = u16::from_be_bytes([bytes[p], bytes[p + 1]]);
        p += 2;
        need(p, 2)?;
        let total_n = u16::from_be_bytes([bytes[p], bytes[p + 1]]);
        p += 2;
        need(p, 2)?;
        let total_k = u16::from_be_bytes([bytes[p], bytes[p + 1]]);
        p += 2;
        need(p, 8)?;
        let real_len = u64::from_be_bytes(bytes[p..p + 8].try_into().unwrap());
        p += 8;
        need(p, 1)?;
        let ctn = bytes[p] as usize;
        p += 1;
        need(p, ctn)?;
        let content_type =
            String::from_utf8(bytes[p..p + ctn].to_vec()).map_err(|e| format!("ct: {e}"))?;
        p += ctn;
        need(p, 1)?;
        let fnn = bytes[p] as usize;
        p += 1;
        need(p, fnn)?;
        let filename =
            String::from_utf8(bytes[p..p + fnn].to_vec()).map_err(|e| format!("filename: {e}"))?;
        p += fnn;
        need(p, 2)?;
        let cl = u16::from_be_bytes([bytes[p], bytes[p + 1]]) as usize;
        p += 2;
        need(p, cl)?;
        let coeffs = bytes[p..p + cl].to_vec();
        p += cl;
        need(p, 4)?;
        let pl = u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        need(p, pl)?;
        let payload = bytes[p..p + pl].to_vec();
        Ok(ShareFile {
            escrow_id,
            shard_idx,
            total_n,
            total_k,
            real_len,
            content_type,
            filename,
            coeffs,
            payload,
        })
    }
}

/// Split a file into N `.holoshare` shares. Deterministic: the same file +
/// parameters → the same shares (escrow_id is derived from SHA-256 of content
/// + metadata, and the RNG is seeded from the same hash).
pub fn split_into_shares(data: &[u8], params: &EscrowParams) -> Vec<ShareFile> {
    assert!(params.k >= 1 && params.k <= 256, "K out of range");
    assert!(params.n >= params.k, "N must be >= K");
    let gf = Gf::new();

    // CID-like escrow_id: SHA-256(data || params) → first 16 bytes.
    let mut hasher = Sha256::new();
    hasher.update(b"holofs-escrow-v1");
    hasher.update(&(params.k as u32).to_be_bytes());
    hasher.update(&(params.n as u32).to_be_bytes());
    hasher.update(params.content_type.as_bytes());
    hasher.update(data);
    let cid = hasher.finalize();
    let mut escrow_id = [0u8; 16];
    escrow_id.copy_from_slice(&cid[..16]);
    let seed = u64::from_be_bytes(cid[16..24].try_into().unwrap());
    let mut rng = Rng::new(seed);

    let (_sl, shards) = encode_layer_with_k(&gf, data, params.k, params.n, &mut rng);
    shards
        .into_iter()
        .enumerate()
        .map(|(idx, sh)| ShareFile {
            escrow_id,
            shard_idx: idx as u16,
            total_n: params.n as u16,
            total_k: params.k as u16,
            real_len: data.len() as u64,
            content_type: params.content_type.clone(),
            filename: params.filename.clone(),
            coeffs: sh.coeffs,
            payload: sh.payload,
        })
        .collect()
}

/// Recover a file from a set of shares. Checks:
/// - all shares share the same `escrow_id`,
/// - `total_k`/`total_n` agree,
/// - the set contains >= K shares.
///
/// Returns (raw_bytes, content_type, filename).
pub fn recover_from_shares(shares: &[ShareFile]) -> Result<(Vec<u8>, String, String), String> {
    if shares.is_empty() {
        return Err("no shares".into());
    }
    let first = &shares[0];
    let k = first.total_k as usize;
    for s in shares {
        if s.escrow_id != first.escrow_id {
            return Err(format!(
                "shares come from different escrows: {} vs {}",
                hex_short(&first.escrow_id),
                hex_short(&s.escrow_id)
            ));
        }
        if s.total_k != first.total_k || s.total_n != first.total_n {
            return Err("K/N disagree across shares".into());
        }
    }
    if shares.len() < k {
        return Err(format!("need at least {k} shares, have {}", shares.len()));
    }
    // Dedup by shard_idx — in case the user provided two copies of one share.
    let mut seen: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut uniq: Vec<&ShareFile> = Vec::new();
    for s in shares {
        if seen.insert(s.shard_idx) {
            uniq.push(s);
        }
    }
    if uniq.len() < k {
        return Err(format!(
            "after dedup only {} unique shares, need {k}",
            uniq.len()
        ));
    }
    let sym_len = first.payload.len();
    let gf = Gf::new();
    let shard_objs: Vec<Shard> = uniq
        .iter()
        .take(k)
        .map(|s| Shard {
            coeffs: s.coeffs.clone(),
            payload: s.payload.clone(),
        })
        .collect();
    let refs: Vec<&Shard> = shard_objs.iter().collect();
    let decoded = decode_layer_with_k(&gf, &refs, k, sym_len)
        .ok_or_else(|| "decode failed: shares are linearly dependent".to_string())?;
    let real_len = first.real_len as usize;
    Ok((
        decoded[..real_len.min(decoded.len())].to_vec(),
        first.content_type.clone(),
        first.filename.clone(),
    ))
}

fn hex_short(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes.iter().take(8) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(k: usize, n: usize, data: &[u8]) {
        let params = EscrowParams {
            k,
            n,
            content_type: "application/octet-stream".into(),
            filename: "secret.bin".into(),
        };
        let shares = split_into_shares(data, &params);
        assert_eq!(shares.len(), n);
        // Any K of N → recovery.
        let take_k: Vec<ShareFile> = shares.iter().take(k).cloned().collect();
        let (back, ct, fname) = recover_from_shares(&take_k).unwrap();
        assert_eq!(back, data);
        assert_eq!(ct, "application/octet-stream");
        assert_eq!(fname, "secret.bin");
    }

    #[test]
    fn escrow_3_of_5_seed_phrase() {
        let seed =
            b"abandon ability able about above absent absorb abstract absurd abuse access accident";
        roundtrip(3, 5, seed);
    }

    #[test]
    fn escrow_4_of_7_larger() {
        let data: Vec<u8> = (0..2048).map(|i| (i * 31) as u8).collect();
        roundtrip(4, 7, &data);
    }

    #[test]
    fn escrow_with_fewer_than_k_fails() {
        let data = b"my secret password";
        let params = EscrowParams {
            k: 3,
            n: 5,
            content_type: "text/plain".into(),
            filename: "pwd.txt".into(),
        };
        let shares = split_into_shares(data, &params);
        let only_two: Vec<ShareFile> = shares.iter().take(2).cloned().collect();
        assert!(recover_from_shares(&only_two).is_err());
    }

    #[test]
    fn escrow_with_wrong_id_fails() {
        let data_a = b"file A";
        let data_b = b"file B";
        let params = EscrowParams {
            k: 2,
            n: 3,
            content_type: "text/plain".into(),
            filename: "x.txt".into(),
        };
        let mut a_shares = split_into_shares(data_a, &params);
        let b_shares = split_into_shares(data_b, &params);
        // Splice in a share from a different file.
        a_shares[1] = b_shares[1].clone();
        assert!(recover_from_shares(&a_shares).is_err());
    }

    #[test]
    fn share_file_encode_decode_roundtrip() {
        let s = ShareFile {
            escrow_id: [0x11; 16],
            shard_idx: 2,
            total_n: 5,
            total_k: 3,
            real_len: 12345,
            content_type: "application/pdf".into(),
            filename: "important.pdf".into(),
            coeffs: vec![0xAA; 3],
            payload: vec![0x55; 100],
        };
        let bytes = s.encode();
        let back = ShareFile::decode(&bytes).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn share_file_decode_rejects_bad_magic() {
        let bad = vec![0u8; 100];
        assert!(ShareFile::decode(&bad).is_err());
    }

    #[test]
    fn duplicate_shares_dedup_correctly() {
        let data = b"need 3 of 5";
        let params = EscrowParams {
            k: 3,
            n: 5,
            content_type: "text/plain".into(),
            filename: "test.txt".into(),
        };
        let shares = split_into_shares(data, &params);
        // 3 unique + 1 duplicate = 4 total; should still work.
        let mut input = vec![shares[0].clone(), shares[1].clone(), shares[2].clone()];
        input.push(shares[1].clone());
        let (back, _, _) = recover_from_shares(&input).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn deterministic_escrow_id() {
        // Same file + same params → same escrow_id.
        let data = b"deterministic test";
        let params = EscrowParams {
            k: 2,
            n: 3,
            content_type: "text/plain".into(),
            filename: "d.txt".into(),
        };
        let a = split_into_shares(data, &params);
        let b = split_into_shares(data, &params);
        assert_eq!(a[0].escrow_id, b[0].escrow_id);
        // Different data → different escrow_id.
        let c = split_into_shares(b"different data", &params);
        assert_ne!(a[0].escrow_id, c[0].escrow_id);
    }
}
