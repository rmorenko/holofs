//! Stage 7.4 (variant A): admin whitelist.
//!
//! Sybil resistance is rooted in trusting the admin: only nodes whose
//! `(address, pubkey, zone)` is signed with the admin key count as cluster
//! members. Any node outside the whitelist is ignored by placement and
//! rejected on handshake.
//!
//! This is **the same guarantee as Backblaze/Ceph**: trust rooted in the
//! admin. It does not defend against an admin standing up 100 nodes itself;
//! it does defend against anyone without the admin key.
//!
//! Whitelist format (binary, with a fixed wire format):
//! ```text
//! magic        8  bytes = b"HOLOFSW1"
//! n_entries    4  bytes BE
//! repeated n_entries times:
//!   addr_len   2  bytes BE
//!   addr_str   addr_len bytes UTF-8
//!   pubkey     32 bytes (Ed25519)
//!   zone       1  byte
//! admin_pubkey 32 bytes
//! admin_sig    64 bytes Ed25519, signing everything above under domain
//!              `holofs-whitelist-v1`
//! ```
//!
//! `admin_pubkey` is embedded in the file (out-of-band the client must match
//! it against an expected key — this is TOFU/preloaded trust anchor). The
//! signature covers everything before it including `admin_pubkey`, so the
//! admin key cannot be swapped without re-signing.

use std::io;

use crate::identity::{
    verify_blob, NodeIdentity, PubKey, Sig, DOMAIN_WHITELIST, PUBKEY_LEN, SIG_LEN,
};

const MAGIC: &[u8; 8] = b"HOLOFSW1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhitelistEntry {
    pub addr: String,
    pub pubkey: PubKey,
    pub zone: u8,
}

/// Signed cluster whitelist: a set of `(addr, pubkey, zone)` + admin pubkey +
/// signature. Distributed to clients and auditors as the "canonical cluster
/// composition at time X". A membership change → a new signed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whitelist {
    pub entries: Vec<WhitelistEntry>,
    pub admin_pubkey: PubKey,
    pub signature: Sig,
}

impl Whitelist {
    /// Build and sign a whitelist using the admin identity.
    pub fn sign(entries: Vec<WhitelistEntry>, admin: &NodeIdentity) -> Self {
        let admin_pubkey = admin.pubkey();
        let blob = encode_signed_blob(&entries, &admin_pubkey);
        let signature = admin.sign_blob(DOMAIN_WHITELIST, &blob);
        Self {
            entries,
            admin_pubkey,
            signature,
        }
    }

    /// Verify the signature. If `expected_admin` is set, additionally check
    /// that `admin_pubkey` matches it (defence against admin substitution).
    pub fn verify(&self, expected_admin: Option<&PubKey>) -> bool {
        if let Some(exp) = expected_admin {
            if &self.admin_pubkey != exp {
                return false;
            }
        }
        let blob = encode_signed_blob(&self.entries, &self.admin_pubkey);
        verify_blob(&self.admin_pubkey, DOMAIN_WHITELIST, &blob, &self.signature)
    }

    /// Look up an entry by address.
    pub fn lookup_by_addr(&self, addr: &str) -> Option<&WhitelistEntry> {
        self.entries.iter().find(|e| e.addr == addr)
    }

    /// Look up an entry by pubkey.
    pub fn lookup_by_pubkey(&self, pk: &PubKey) -> Option<&WhitelistEntry> {
        self.entries.iter().find(|e| &e.pubkey == pk)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&encode_signed_blob(&self.entries, &self.admin_pubkey));
        b.extend_from_slice(&self.signature);
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < 8 + 4 + PUBKEY_LEN + SIG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "whitelist truncated",
            ));
        }
        if &buf[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a holofs whitelist",
            ));
        }
        let mut pos = 8usize;
        let n = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            if pos + 2 > buf.len() {
                return Err(eof());
            }
            let alen = u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + alen + PUBKEY_LEN + 1 > buf.len() {
                return Err(eof());
            }
            let addr = String::from_utf8(buf[pos..pos + alen].to_vec())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("addr: {e}")))?;
            pos += alen;
            let mut pubkey = [0u8; PUBKEY_LEN];
            pubkey.copy_from_slice(&buf[pos..pos + PUBKEY_LEN]);
            pos += PUBKEY_LEN;
            let zone = buf[pos];
            pos += 1;
            entries.push(WhitelistEntry { addr, pubkey, zone });
        }
        if pos + PUBKEY_LEN + SIG_LEN != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "whitelist: extra or missing trailing bytes",
            ));
        }
        let mut admin_pubkey = [0u8; PUBKEY_LEN];
        admin_pubkey.copy_from_slice(&buf[pos..pos + PUBKEY_LEN]);
        pos += PUBKEY_LEN;
        let mut signature = [0u8; SIG_LEN];
        signature.copy_from_slice(&buf[pos..pos + SIG_LEN]);
        Ok(Whitelist {
            entries,
            admin_pubkey,
            signature,
        })
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "whitelist truncated")
}

/// Canonical serialised blob of "what is signed": the entries list plus
/// `admin_pubkey`. The signature itself is naturally not included.
fn encode_signed_blob(entries: &[WhitelistEntry], admin_pubkey: &PubKey) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        let ab = e.addr.as_bytes();
        b.extend_from_slice(&(ab.len() as u16).to_be_bytes());
        b.extend_from_slice(ab);
        b.extend_from_slice(&e.pubkey);
        b.push(e.zone);
    }
    b.extend_from_slice(admin_pubkey);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(addr: &str, seed: u8, zone: u8) -> WhitelistEntry {
        WhitelistEntry {
            addr: addr.into(),
            pubkey: [seed; PUBKEY_LEN],
            zone,
        }
    }

    #[test]
    fn whitelist_roundtrip() {
        let admin = NodeIdentity::generate();
        let wl = Whitelist::sign(
            vec![
                entry("127.0.0.1:5000", 1, 0),
                entry("127.0.0.1:5001", 2, 0),
                entry("127.0.0.1:5002", 3, 1),
            ],
            &admin,
        );
        let bytes = wl.encode();
        let back = Whitelist::decode(&bytes).unwrap();
        assert_eq!(back, wl);
        assert!(back.verify(Some(&admin.pubkey())));
    }

    #[test]
    fn whitelist_signature_verifies() {
        let admin = NodeIdentity::generate();
        let wl = Whitelist::sign(vec![entry("a", 1, 0), entry("b", 2, 1)], &admin);
        assert!(wl.verify(None));
        assert!(wl.verify(Some(&admin.pubkey())));
    }

    #[test]
    fn whitelist_rejects_wrong_admin() {
        let admin = NodeIdentity::generate();
        let attacker = NodeIdentity::generate();
        let wl = Whitelist::sign(vec![entry("a", 1, 0)], &admin);
        assert!(!wl.verify(Some(&attacker.pubkey())));
    }

    #[test]
    fn whitelist_detects_tampering_with_entries() {
        let admin = NodeIdentity::generate();
        let mut wl = Whitelist::sign(vec![entry("a", 1, 0)], &admin);
        // Substituting a node's pubkey after signing must break verification.
        wl.entries[0].pubkey = [0xFF; PUBKEY_LEN];
        assert!(!wl.verify(None));
    }

    #[test]
    fn whitelist_detects_admin_pubkey_substitution() {
        // Attacker swaps `admin_pubkey` in the whitelist for their own while
        // keeping the original signature. The signature covers admin_pubkey →
        // verification must fail.
        let admin = NodeIdentity::generate();
        let attacker = NodeIdentity::generate();
        let mut wl = Whitelist::sign(vec![entry("a", 1, 0)], &admin);
        wl.admin_pubkey = attacker.pubkey();
        assert!(!wl.verify(None));
    }

    #[test]
    fn whitelist_lookup_by_addr_and_pubkey() {
        let admin = NodeIdentity::generate();
        let e1 = entry("127.0.0.1:5000", 7, 0);
        let e2 = entry("127.0.0.1:5001", 8, 1);
        let wl = Whitelist::sign(vec![e1.clone(), e2.clone()], &admin);
        assert_eq!(wl.lookup_by_addr("127.0.0.1:5000"), Some(&e1));
        assert_eq!(wl.lookup_by_pubkey(&[8; PUBKEY_LEN]), Some(&e2));
        assert_eq!(wl.lookup_by_addr("missing"), None);
    }
}
