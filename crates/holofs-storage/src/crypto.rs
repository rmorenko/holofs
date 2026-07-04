//! at-rest shard encryption for [`crate::Store`].
//!
//! Every shard file's payload (RLNC coefficients + encoded chunk
//! bytes) is optionally sealed with AES-256-GCM before hitting disk.
//! Header fields — magic, object_id, channel, layer, lengths — stay
//! in plaintext so `Store::open` can index without the key. The
//! sensitive part is the payload; that's what an on-disk snapshot
//! attacker (threat I2 in [`docs/threat-model.md`]) can read from a
//! v1 file.
//!
//! ## Key derivation
//!
//! The per-node encryption key is derived from the node's
//! [`crate::identity::NodeIdentity`] via HKDF-SHA256:
//!
//! ```text
//! seed = SigningKey::to_bytes()   // 32 bytes (Ed25519 secret)
//! prk  = HKDF-Extract(salt=b"holofs-shard-salt-v1", ikm=seed)
//! key  = HKDF-Expand(prk, info=b"holofs-shard-key-v1", L=32)
//! ```
//!
//! No new key management burden: the operator already owns
//! `identity.key`; losing it already loses the node's identity.
//! At-rest encryption piggybacks on that.
//!
//! ## Wire format (per shard file)
//!
//! Preamble is the same 8-byte magic + 18-byte header
//! [`crate::node_service::write_shard_file`] already writes. On
//! disk the magic distinguishes the two formats:
//!
//! - `HOLOFSS1` — plaintext (pre-). Payload = coeffs || payload.
//! - `HOLOFSS2` — sealed (). Layout:
//!   ```text
//!   [ 8 B magic "HOLOFSS2" ]
//!   [ 18 B header (object_id, c, l, lens) ]  <-- plaintext, AAD to GCM
//!   [ 12 B nonce ]                            <-- random per shard
//!   [ ciphertext(coeffs || payload) ]         <-- includes 16-byte tag
//!   ```
//!
//! Read path sniffs the magic and dispatches; writers pick v1 or v2
//! at Store construction time based on the key being present.
//!
//! ## Threat model
//!
//! - **In scope**: an adversary snapshots the shard files off a
//!   powered-off node (backup leak, decommissioned disk, RAID rebuild
//!   left the old drive readable). Without `identity.key` they see
//!   opaque ciphertext.
//! - **Out of scope**: an adversary with root on a running node.
//!   Once the node process is up, the derived key is in RAM and
//!   `read_shard_file` produces plaintext for legitimate audits.
//!
//! ## Rotation
//!
//! Not supported in . Rewriting every shard under a new key is
//! a rebuild-scale operation; the recommendation is to spawn a
//! fresh node with a fresh identity and let the auto-repair pass
//! rebalance shards onto it.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;

/// Length of the AES-256-GCM key.
pub const KEY_LEN: usize = 32;
/// Length of a GCM nonce.
pub const NONCE_LEN: usize = 12;
/// Length of a GCM authentication tag.
pub const TAG_LEN: usize = 16;

/// Salt for the HKDF Extract step. Bump the version suffix if the
/// derivation semantics change (e.g. new info string, different
/// hash). Value is hard-coded — never negotiated over the wire.
const HKDF_SALT: &[u8] = b"holofs-shard-salt-v1";

/// Info string for the HKDF Expand step. Also hard-coded, versioned
/// alongside `HKDF_SALT`.
const HKDF_INFO: &[u8] = b"holofs-shard-key-v1";

/// Derive the 32-byte shard-encryption key from raw node identity
/// material. `identity_material` is expected to be the
/// [`ed25519_dalek::SigningKey::to_bytes()`] output — the 32-byte
/// Ed25519 secret seed.
///
/// The salt and info strings are hard-coded so two nodes with the
/// same identity always derive the same key (they never should in
/// practice — identity is per-node — but this makes the derivation
/// deterministic and testable).
pub fn derive_shard_key(identity_material: &[u8]) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), identity_material);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("HKDF expand of 32 bytes never fails");
    okm
}

/// Encrypt `plaintext` under `key` using AES-256-GCM with a fresh
/// random 12-byte nonce. `aad` (additional authenticated data)
/// binds the ciphertext to the shard header (magic + object_id +
/// channel + layer + lengths) — a header rewrite invalidates the
/// tag on decrypt.
///
/// Layout of the returned Vec:
/// `[nonce (12 B)] [ciphertext] [tag (16 B, appended by aes-gcm)]`.
pub fn encrypt(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("AES-GCM encrypt only fails on catastrophic key issue");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt a `[nonce | ciphertext+tag]` blob produced by
/// [`encrypt`]. Returns `Ok(plaintext)` on success and a plain
/// `String` error on any of: truncated input, bad tag, key mismatch,
/// AAD mismatch. The caller (typically `read_shard_file`) usually
/// wraps this into an [`std::io::Error`].
pub fn decrypt(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    blob: &[u8],
) -> Result<Vec<u8>, String> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(format!(
            "sealed shard truncated: {} bytes, need ≥ {}",
            blob.len(),
            NONCE_LEN + TAG_LEN
        ));
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload {
                msg: ct,
                aad,
            },
        )
        .map_err(|e| format!("AES-GCM decrypt failed (bad key / tampered file): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic() {
        let seed = [42u8; 32];
        let k1 = derive_shard_key(&seed);
        let k2 = derive_shard_key(&seed);
        assert_eq!(k1, k2);
        // Different seed → different key.
        let seed2 = [43u8; 32];
        let k3 = derive_shard_key(&seed2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = derive_shard_key(&[0xAB; 32]);
        let aad = b"HOLOFSS2\x00\x00\x00\x00\x00\x00\x00\x2A\x01\x02\x00\x00\x00\x08\x00\x00\x00\x10";
        let plaintext = b"here is some shard payload bytes";
        let sealed = encrypt(&key, aad, plaintext);
        assert!(sealed.len() > plaintext.len(), "must include nonce + tag");
        let back = decrypt(&key, aad, &sealed).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn decrypt_rejects_wrong_key() {
        let k1 = derive_shard_key(&[0xAB; 32]);
        let k2 = derive_shard_key(&[0xCD; 32]);
        let aad = b"aad";
        let sealed = encrypt(&k1, aad, b"payload");
        assert!(decrypt(&k2, aad, &sealed).is_err());
    }

    #[test]
    fn decrypt_rejects_wrong_aad() {
        let key = derive_shard_key(&[0xAB; 32]);
        let sealed = encrypt(&key, b"header-a", b"payload");
        // Tampered AAD (simulating header rewrite) invalidates the tag.
        assert!(decrypt(&key, b"header-b", &sealed).is_err());
    }

    #[test]
    fn decrypt_rejects_truncated_input() {
        let key = derive_shard_key(&[0xAB; 32]);
        let sealed = encrypt(&key, b"aad", b"payload");
        // Cut off part of the tag.
        assert!(decrypt(&key, b"aad", &sealed[..sealed.len() - 5]).is_err());
    }

    #[test]
    fn encrypt_uses_fresh_nonce_each_call() {
        let key = derive_shard_key(&[0xAB; 32]);
        let a = encrypt(&key, b"aad", b"payload");
        let b = encrypt(&key, b"aad", b"payload");
        // With random 12-byte nonces the two outputs must differ
        // even for identical input.
        assert_ne!(a, b);
    }
}
