//! Stage 7.3: Ed25519 node identity.
//!
//! On startup each node either generates a keypair or loads one from
//! `<storage_dir>/identity.key`. The public key is distributed via
//! [`crate::whitelist::Whitelist`] (Stage 7.4), which doubles as the
//! "single source of truth" about cluster membership.
//!
//! Handshake (optional, on top of the existing wire protocol):
//! - client → `Request::AuthChallenge { nonce: [u8; 32] }`
//! - node → `Response::AuthChallengeOk { signature: [u8; 64] }` where
//!   `signature = sign(node_secret, domain_prefix || nonce)`
//! - client verifies the signature against `whitelist[node].pubkey`.
//!
//! `domain_prefix = b"holofs-auth-v1"` — guards against cross-protocol reuse:
//! the same signature cannot be replayed in another context (e.g. as a
//! whitelist signature).
//!
//! This is the **identity** layer: the guarantee that "node 7 is the one
//! to whom the admin issued this key". On top of it sits the whitelist
//! (Stage 7.4) — the guarantee that "this key is actually a cluster member".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Length of an Ed25519 public key in bytes.
pub const PUBKEY_LEN: usize = 32;
/// Length of an Ed25519 signature in bytes.
pub const SIG_LEN: usize = 64;
/// Length of a challenge nonce in bytes.
pub const NONCE_LEN: usize = 32;

/// Domain prefix for challenge signatures. Bump the major version on any
/// change to signed-message semantics (e.g. if we ever include a timestamp).
pub const DOMAIN_AUTH: &[u8] = b"holofs-auth-v1";

/// Domain prefix for signing whitelist blobs.
pub const DOMAIN_WHITELIST: &[u8] = b"holofs-whitelist-v1";

pub type PubKey = [u8; PUBKEY_LEN];
pub type Sig = [u8; SIG_LEN];
pub type Nonce = [u8; NONCE_LEN];

/// Node identity: keypair plus the public key serving as the node identifier.
#[derive(Clone)]
pub struct NodeIdentity {
    signing: SigningKey,
}

impl NodeIdentity {
    /// Generate a fresh random identity.
    pub fn generate() -> Self {
        use rand_core::OsRng;
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// Load identity from a file (32-byte seed), or create a new one and
    /// persist it. The file is a plain 32-byte seed; the format is stable.
    pub fn load_or_create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path: PathBuf = path.as_ref().to_path_buf();
        match fs::read(&path) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut seed = [0u8; 32];
                seed.copy_from_slice(&bytes);
                Ok(Self {
                    signing: SigningKey::from_bytes(&seed),
                })
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "identity file must be exactly 32 bytes",
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Self::generate();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Atomic write: tmp + rename. Permissions 0600 — it's a secret.
                let tmp = path.with_extension("key.tmp");
                fs::write(&tmp, id.signing.to_bytes())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = fs::metadata(&tmp)?.permissions();
                    perm.set_mode(0o600);
                    fs::set_permissions(&tmp, perm)?;
                }
                fs::rename(&tmp, &path)?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    pub fn pubkey(&self) -> PubKey {
        self.signing.verifying_key().to_bytes()
    }

    /// Sign `nonce` under the `holofs-auth-v1` domain prefix. This is the
    /// response to the handshake challenge.
    pub fn sign_challenge(&self, nonce: &Nonce) -> Sig {
        let mut msg = Vec::with_capacity(DOMAIN_AUTH.len() + NONCE_LEN);
        msg.extend_from_slice(DOMAIN_AUTH);
        msg.extend_from_slice(nonce);
        self.signing.sign(&msg).to_bytes()
    }

    /// Sign an arbitrary blob (used by the whitelist).
    pub fn sign_blob(&self, domain: &[u8], blob: &[u8]) -> Sig {
        let mut msg = Vec::with_capacity(domain.len() + blob.len());
        msg.extend_from_slice(domain);
        msg.extend_from_slice(blob);
        self.signing.sign(&msg).to_bytes()
    }
}

/// Verify a challenge signature against a known pubkey.
pub fn verify_challenge(pubkey: &PubKey, nonce: &Nonce, sig: &Sig) -> bool {
    verify_blob(pubkey, DOMAIN_AUTH, nonce, sig)
}

/// Verify a signature over an arbitrary blob under a domain prefix.
pub fn verify_blob(pubkey: &PubKey, domain: &[u8], blob: &[u8], sig: &Sig) -> bool {
    let vk = match VerifyingKey::from_bytes(pubkey) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let signature = Signature::from_bytes(sig);
    let mut msg = Vec::with_capacity(domain.len() + blob.len());
    msg.extend_from_slice(domain);
    msg.extend_from_slice(blob);
    vk.verify(&msg, &signature).is_ok()
}

/// Generate a random nonce. Used by clients to issue a challenge.
pub fn fresh_nonce() -> Nonce {
    use rand_core::OsRng;
    use rand_core::RngCore;
    let mut n = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_roundtrip_passes() {
        let id = NodeIdentity::generate();
        let pk = id.pubkey();
        let nonce = fresh_nonce();
        let sig = id.sign_challenge(&nonce);
        assert!(verify_challenge(&pk, &nonce, &sig));
    }

    #[test]
    fn challenge_with_wrong_nonce_fails() {
        let id = NodeIdentity::generate();
        let pk = id.pubkey();
        let n1 = fresh_nonce();
        let n2 = fresh_nonce();
        let sig = id.sign_challenge(&n1);
        assert!(!verify_challenge(&pk, &n2, &sig));
    }

    #[test]
    fn challenge_with_wrong_pubkey_fails() {
        let id1 = NodeIdentity::generate();
        let id2 = NodeIdentity::generate();
        let nonce = fresh_nonce();
        let sig = id1.sign_challenge(&nonce);
        assert!(!verify_challenge(&id2.pubkey(), &nonce, &sig));
    }

    #[test]
    fn domain_separation_blocks_cross_protocol_reuse() {
        // A challenge signature must not validate as a whitelist signature
        // and vice versa — even if the nonce equals the blob byte-for-byte.
        let id = NodeIdentity::generate();
        let pk = id.pubkey();
        let bytes = [0xAA; NONCE_LEN];
        let auth_sig = id.sign_challenge(&bytes);
        // The same 32-byte sequence as a "blob" under the whitelist domain.
        assert!(!verify_blob(&pk, DOMAIN_WHITELIST, &bytes, &auth_sig));
        let wl_sig: Sig = id.sign_blob(DOMAIN_WHITELIST, &bytes);
        assert!(!verify_challenge(&pk, &bytes, &wl_sig));
    }

    #[test]
    fn load_or_create_persists_keypair_across_calls() {
        let dir = std::env::temp_dir().join(format!(
            "holofs-id-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("identity.key");
        let id1 = NodeIdentity::load_or_create(&path).unwrap();
        let id2 = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(id1.pubkey(), id2.pubkey());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arbitrary_blob_roundtrip() {
        let id = NodeIdentity::generate();
        let blob = b"arbitrary content";
        let sig = id.sign_blob(b"test-domain", blob);
        assert!(verify_blob(&id.pubkey(), b"test-domain", blob, &sig));
        // Wrong blob — must fail.
        assert!(!verify_blob(&id.pubkey(), b"test-domain", b"other", &sig));
        // Wrong domain — must fail.
        assert!(!verify_blob(&id.pubkey(), b"wrong-domain", blob, &sig));
    }
}
