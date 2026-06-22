//! Content addressing and integrity:
//! - [`shard_hash`] — hash of a single shard (for read-side verification).
//! - [`data_cid`] — hash of source channels + parameters (stable object CID).
//! - [`merkle_root`] — root of a binary tree over leaves (shard hashes).
//!
//! All hashes use domain prefixes — defence against cross-protocol confusion.

use crate::hash::{sha256, Sha256, HASH_LEN};
use crate::rlnc::Shard;

const TAG_DATA: &[u8] = b"holofs-data-v1\0";
const TAG_SHARD: &[u8] = b"holofs-shard-v1\0";
const TAG_MERKLE: &[u8] = b"holofs-merkle-v1\0";

pub type Hash = [u8; HASH_LEN];

pub fn shard_hash(shard: &Shard) -> Hash {
    let mut s = Sha256::new();
    s.update(TAG_SHARD);
    s.update(&(shard.coeffs.len() as u16).to_be_bytes());
    s.update(&(shard.payload.len() as u32).to_be_bytes());
    s.update(&shard.coeffs);
    s.update(&shard.payload);
    s.finalize()
}

/// Stable object CID: hash of the source channels and base parameters.
/// Independent of placement and RLNC randomness — so an identical image yields
/// the same CID across different clients.
pub fn data_cid(channels: &[Vec<f32>], w: usize, h: usize, levels: u8, k: u16) -> Hash {
    let mut s = Sha256::new();
    s.update(TAG_DATA);
    s.update(&(channels.len() as u8).to_be_bytes());
    s.update(&(w as u32).to_be_bytes());
    s.update(&(h as u32).to_be_bytes());
    s.update(&[levels]);
    s.update(&k.to_be_bytes());
    for plane in channels {
        assert_eq!(plane.len(), w * h, "plane does not match w×h");
        // Direct hash of f32 LE bytes — deterministic, endian-independent.
        for v in plane {
            s.update(&v.to_le_bytes());
        }
    }
    s.finalize()
}

/// Root of a binary Merkle tree. On odd count the last hash is duplicated
/// (CV-style padding). Empty set → zero root.
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return [0u8; HASH_LEN];
    }
    let mut level: Vec<Hash> = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        let mut i = 0;
        while i < level.len() {
            let right = if i + 1 < level.len() {
                &level[i + 1]
            } else {
                &level[i]
            };
            next.push(hash_pair(&level[i], right));
            i += 2;
        }
        level = next;
    }
    level[0]
}

fn hash_pair(a: &Hash, b: &Hash) -> Hash {
    let mut buf = Vec::with_capacity(TAG_MERKLE.len() + 64);
    buf.extend_from_slice(TAG_MERKLE);
    buf.extend_from_slice(a);
    buf.extend_from_slice(b);
    sha256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(coeffs: &[u8], payload: &[u8]) -> Shard {
        Shard {
            coeffs: coeffs.to_vec(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn shard_hash_deterministic() {
        let a = s(&[1, 2, 3], &[10, 20, 30]);
        assert_eq!(shard_hash(&a), shard_hash(&a.clone()));
    }

    #[test]
    fn shard_hash_changes_on_payload_flip() {
        let a = s(&[1, 2, 3], &[10, 20, 30]);
        let mut b = a.clone();
        b.payload[1] ^= 1;
        assert_ne!(shard_hash(&a), shard_hash(&b));
    }

    #[test]
    fn shard_hash_changes_on_coeff_flip() {
        let a = s(&[1, 2, 3], &[10, 20, 30]);
        let mut b = a.clone();
        b.coeffs[0] ^= 1;
        assert_ne!(shard_hash(&a), shard_hash(&b));
    }

    #[test]
    fn data_cid_stable_across_calls() {
        let channels = vec![vec![1.0f32, 2.0, 3.0, 4.0]; 3];
        let c1 = data_cid(&channels, 2, 2, 1, 16);
        let c2 = data_cid(&channels, 2, 2, 1, 16);
        assert_eq!(c1, c2);
    }

    #[test]
    fn data_cid_differs_on_param_change() {
        let channels = vec![vec![1.0f32; 4]; 3];
        let c1 = data_cid(&channels, 2, 2, 1, 16);
        let c2 = data_cid(&channels, 2, 2, 1, 32);
        assert_ne!(c1, c2, "different K → different CID");
    }

    #[test]
    fn data_cid_differs_on_content_change() {
        let mut ch1 = vec![vec![1.0f32; 4]; 3];
        let ch2 = ch1.clone();
        ch1[0][1] = 1.0 + f32::EPSILON;
        let c1 = data_cid(&ch1, 2, 2, 1, 16);
        let c2 = data_cid(&ch2, 2, 2, 1, 16);
        assert_ne!(c1, c2);
    }

    #[test]
    fn merkle_root_empty_is_zero() {
        assert_eq!(merkle_root(&[]), [0u8; HASH_LEN]);
    }

    #[test]
    fn merkle_root_single_leaf_is_itself() {
        let leaf = [7u8; HASH_LEN];
        assert_eq!(merkle_root(&[leaf]), leaf);
    }

    #[test]
    fn merkle_root_two_leaves() {
        let a = [1u8; HASH_LEN];
        let b = [2u8; HASH_LEN];
        assert_eq!(merkle_root(&[a, b]), hash_pair(&a, &b));
    }

    #[test]
    fn merkle_root_changes_when_leaf_changes() {
        let leaves: Vec<Hash> = (0..7u8).map(|i| [i; HASH_LEN]).collect();
        let r1 = merkle_root(&leaves);
        let mut tampered = leaves.clone();
        tampered[3][0] ^= 1;
        let r2 = merkle_root(&tampered);
        assert_ne!(r1, r2);
    }

    #[test]
    fn merkle_root_odd_count_handled() {
        let leaves: Vec<Hash> = (0..5u8).map(|i| [i; HASH_LEN]).collect();
        // does not panic and is stable
        assert_eq!(merkle_root(&leaves), merkle_root(&leaves));
    }
}
