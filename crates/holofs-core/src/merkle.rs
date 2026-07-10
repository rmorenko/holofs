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

/// Root of a binary Merkle tree, second-preimage-hardened.
///
/// v2 P1.6 (CVE-2012-2459 class): the prior implementation left
/// `root([A,B,C]) == root([A,B,C,C])` — duplication of the trailing
/// odd leaf collided the two distinct leaf sets. That's a
/// tree-shape ambiguity: an attacker who knew a valid `(A,B,C)`
/// set could exhibit a padded `(A,B,C,C)` set with the same root
/// and pass it off as authentic.
///
/// Two defences applied together:
///
/// 1. **Domain-separate leaves from interior nodes.** Every leaf is
///    wrapped with a `TAG_LEAF` prefix before entering the tree.
///    A raw shard hash on the wire (which lives in the plain leaf
///    domain) can no longer collide with an interior-node hash
///    (which lives in the `TAG_MERKLE` domain — see `hash_pair`).
///
/// 2. **Bind the leaf count into the root.** After the tree is
///    reduced to a single hash, we finalise `root ← SHA256(TAG_ROOT
///    || leaf_count_be || tree_root)`. Two leaf sets of different
///    length produce two different roots even before the tree walk
///    starts, so the `(A,B,C)` vs `(A,B,C,C)` collision above is
///    impossible.
///
/// Empty set → zero root.
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return [0u8; HASH_LEN];
    }
    let mut level: Vec<Hash> = leaves.iter().map(hash_leaf).collect();
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
    // Mix leaf count into the root — see docstring #2.
    let mut buf = Vec::with_capacity(TAG_ROOT.len() + 8 + HASH_LEN);
    buf.extend_from_slice(TAG_ROOT);
    buf.extend_from_slice(&(leaves.len() as u64).to_be_bytes());
    buf.extend_from_slice(&level[0]);
    sha256(&buf)
}

const TAG_LEAF: &[u8] = b"holofs-merkle-leaf-v1\0";
const TAG_ROOT: &[u8] = b"holofs-merkle-root-v1\0";

fn hash_leaf(leaf: &Hash) -> Hash {
    let mut buf = Vec::with_capacity(TAG_LEAF.len() + HASH_LEN);
    buf.extend_from_slice(TAG_LEAF);
    buf.extend_from_slice(leaf);
    sha256(&buf)
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
    fn merkle_root_single_leaf_is_deterministic() {
        // Post-P1.6: root is no longer identity even at a single
        // leaf (leaf-domain tag + leaf-count mix). We only require
        // that it's stable across calls and non-zero for a
        // non-zero leaf.
        let leaf = [7u8; HASH_LEN];
        let r = merkle_root(&[leaf]);
        assert_eq!(r, merkle_root(&[leaf]));
        assert_ne!(r, [0u8; HASH_LEN]);
    }

    #[test]
    fn merkle_root_two_leaves_is_deterministic() {
        // Post-P1.6: no longer equal to `hash_pair(a, b)` because
        // leaves get domain-tagged and the root is finalised with
        // the leaf count. Stability + non-collision with the
        // single-leaf case is what we check.
        let a = [1u8; HASH_LEN];
        let b = [2u8; HASH_LEN];
        let r = merkle_root(&[a, b]);
        assert_eq!(r, merkle_root(&[a, b]));
        assert_ne!(r, merkle_root(&[a]));
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

    /// P1.6 regression (CVE-2012-2459 class): a set with the trailing
    /// leaf duplicated must NOT collide with the original. Prior to
    /// TAG_LEAF + leaf-count mixing, `root([A,B,C]) == root([A,B,C,C])`
    /// because the odd-count padding rule internally duplicated the
    /// tail anyway.
    #[test]
    fn merkle_root_rejects_odd_leaf_duplication() {
        let three: Vec<Hash> = (0..3u8).map(|i| [i; HASH_LEN]).collect();
        let mut four = three.clone();
        four.push(three[2]); // duplicate the odd-tail leaf
        assert_ne!(merkle_root(&three), merkle_root(&four));
    }

    /// P1.6 regression: leaf count is mixed into the root, so two
    /// distinct-length sets with the same left-projection give
    /// distinct roots.
    #[test]
    fn merkle_root_binds_leaf_count() {
        let short: Vec<Hash> = (0..2u8).map(|i| [i; HASH_LEN]).collect();
        let mut long = short.clone();
        long.push([0u8; HASH_LEN]); // append a zero leaf
        assert_ne!(merkle_root(&short), merkle_root(&long));
    }
}
