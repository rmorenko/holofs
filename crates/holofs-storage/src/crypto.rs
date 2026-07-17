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
//! ## Rotation (P1.7 — envelope encryption)
//!
//! Post-P1.7 the shard key is a **DEK** stored inside a keyring
//! file, wrapped under a separate **KEK** loaded from an operator-
//! chosen source ([`KekSource`]). Rotation appends a fresh DEK to
//! the keyring; existing shards stay readable because decrypt tries
//! each DEK in newest-first order and uses the AES-GCM tag as the
//! natural per-key selector (128-bit tag ⇒ wrong-DEK success rate
//! is negligible). No wire format change, no shard rewrite required
//! at rotation time.
//!
//! Env-var summary (see [`crate::node_service::NodeConfig`] for the
//! full set):
//!
//! | Variable                        | Values                    | Default            |
//! |---------------------------------|---------------------------|--------------------|
//! | `HOLOFS_AT_REST_KEK_SOURCE`     | `identity`, `file`, `env` | `identity`         |
//! | `HOLOFS_AT_REST_KEK_PATH`       | path to 32 raw bytes      | (required for `file`) |
//! | `HOLOFS_AT_REST_KEK_HEX`        | 64 hex chars              | (required for `env`)  |
//! | `HOLOFS_KEYRING_PATH`           | path                      | `<storage>/keyring.json` |
//!
//! Legacy: an install that only ever ran with the pre-P1.7
//! identity-derived key materialises a bootstrap keyring on first
//! open, with `id=0` and DEK bytes equal to the old HKDF output.
//! Existing sealed shards decrypt untouched under `id=0`.

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

// === P1.7 envelope encryption =====================================

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the per-node **Key Encryption Key** comes from. The KEK
/// wraps every DEK in the keyring; a compromise of the KEK source
/// is what an attacker needs to decrypt any shard the node has
/// ever written.
///
/// - [`KekSource::IdentitySeed`] — pre-P1.7 default. HKDF the
///   node's `identity.key`; convenient for single-tenant / dev
///   deployments where the identity file already lives on a
///   secured mount. Compromise-of-identity = compromise-of-KEK,
///   which is the caveat P1.7 exists to break for stricter
///   operators.
/// - [`KekSource::File`] — read 32 raw bytes from a filesystem
///   path. The intended prod pattern: mount a Kubernetes Secret
///   / HashiCorp Vault agent output at `--tls-cert`-style locations,
///   separate from the identity file, ideally on tmpfs.
/// - [`KekSource::EnvHex`] — 64-hex-char value from an env var.
///   For CI / one-shot benchmarks where injecting a file mount
///   is more friction than it's worth. NOT recommended for prod
///   (env vars leak into process listings, crash dumps, journald).
#[derive(Debug, Clone)]
pub enum KekSource {
    IdentitySeed,
    File(PathBuf),
    EnvHex(String),
}

impl KekSource {
    /// Materialise the KEK bytes. Called once at boot; the raw KEK
    /// is used to unwrap keyring entries and then dropped from
    /// memory (the DEKs live for the process lifetime).
    ///
    /// `identity_seed` is only consulted for the `IdentitySeed`
    /// variant — pass the same bytes as `derive_shard_key`.
    pub fn load(&self, identity_seed: &[u8]) -> io::Result<[u8; KEY_LEN]> {
        match self {
            KekSource::IdentitySeed => Ok(derive_shard_key(identity_seed)),
            KekSource::File(path) => {
                let bytes = fs::read(path)?;
                if bytes.len() != KEY_LEN {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "KEK file {} has {} bytes, expected {KEY_LEN}",
                            path.display(),
                            bytes.len()
                        ),
                    ));
                }
                let mut out = [0u8; KEY_LEN];
                out.copy_from_slice(&bytes);
                Ok(out)
            }
            KekSource::EnvHex(hex) => decode_hex_key(hex).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("KEK env-hex: {e}"))
            }),
        }
    }

    /// Diagnostic label — safe to log at boot. Does NOT include the
    /// file path (that's often the operator's private info via a
    /// bind-mount).
    pub fn label(&self) -> &'static str {
        match self {
            KekSource::IdentitySeed => "identity",
            KekSource::File(_) => "file",
            KekSource::EnvHex(_) => "env",
        }
    }
}

/// One keyring entry — a DEK identified by an ever-increasing `id`,
/// stored on disk as ciphertext under the KEK. `raw_dek` is the
/// plaintext held in RAM after unwrap; NEVER serialised.
#[derive(Debug, Clone)]
pub struct DekEntry {
    pub id: u32,
    /// Unix ms when this DEK was minted. Boot logs use it to show
    /// key age; rotation policy is entirely operator-driven.
    pub created_at_unix_ms: u64,
    /// DEK ciphertext = KEK-wrap output. Persisted in `keyring.json`.
    pub wrapped_dek: Vec<u8>,
    /// The 32-byte raw DEK, unwrapped at boot. Kept in memory for
    /// the process lifetime so encrypt / decrypt calls don't
    /// re-hit the KEK path per shard.
    pub raw_dek: [u8; KEY_LEN],
}

/// The keyring — an ordered list of DEK entries plus a pointer to
/// which one to use for NEW writes. Reads try every entry starting
/// from `current_id` and walking backwards.
///
/// On-disk shape (`<storage>/keyring.json`):
///
/// ```json
/// {
///   "format_version": 1,
///   "current_id": 3,
///   "keys": [
///     {"id": 1, "created_at_unix_ms": 1720000000000, "wrapped_dek_hex": "…"},
///     {"id": 2, "created_at_unix_ms": 1725000000000, "wrapped_dek_hex": "…"},
///     {"id": 3, "created_at_unix_ms": 1727500000000, "wrapped_dek_hex": "…"}
///   ]
/// }
/// ```
///
/// The AAD used to wrap each DEK is a fixed constant
/// (`b"holofs-dek-v1"` || `id`) so replaying a wrapped DEK under
/// a different id fails at unwrap time.
pub struct Keyring {
    /// Entries in insertion order (== id order).
    entries: Vec<DekEntry>,
    /// Id of the DEK to use for new writes. Reads walk backwards
    /// from this over `entries`.
    current_id: u32,
}

impl Keyring {
    /// Construct an in-memory keyring holding a single DEK. Used by
    /// [`Store::open_with_key`] as a compatibility shim so existing
    /// callers that pass a raw key don't have to know about the
    /// keyring machinery. Never persisted.
    pub fn in_memory_single(key: [u8; KEY_LEN]) -> Self {
        Self {
            entries: vec![DekEntry {
                id: 1,
                created_at_unix_ms: 0,
                wrapped_dek: Vec::new(),
                raw_dek: key,
            }],
            current_id: 1,
        }
    }

    /// Number of DEKs in the ring.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Is this an empty keyring? Never in practice (open produces
    /// at least one), but kept for symmetry with `len`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Id of the DEK new writes will use.
    pub fn current_id(&self) -> u32 {
        self.current_id
    }

    /// Encrypt `plaintext` under the CURRENT DEK, binding to `aad`.
    /// Fresh random 12-byte nonce per call (see [`encrypt`]).
    pub fn encrypt_current(&self, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let e = self
            .entries
            .iter()
            .find(|e| e.id == self.current_id)
            .expect("keyring current_id must reference an entry");
        encrypt(&e.raw_dek, aad, plaintext)
    }

    /// Decrypt `blob` by trying every entry from newest to oldest.
    /// Returns the plaintext on first successful AES-GCM tag match,
    /// or a summary error listing how many keys were tried on total
    /// failure. Under a 128-bit AES-GCM tag the probability of a
    /// wrong-key tag match is ~2^-128 — indistinguishable from
    /// zero for any realistic keyring size.
    pub fn decrypt_any(&self, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, String> {
        // Iterate current_id-first, then descending. Entries are
        // stored in id order so this is a `rev()` after slicing.
        for e in self.entries.iter().rev() {
            if let Ok(pt) = decrypt(&e.raw_dek, aad, blob) {
                return Ok(pt);
            }
        }
        Err(format!(
            "decrypt failed under all {} DEK(s) in keyring",
            self.entries.len()
        ))
    }

    /// Append a fresh DEK to the ring, wrap under `kek`, bump
    /// `current_id` to the new entry's id. Called by
    /// `holofs-admin rotate-kek`. Persists on the caller side —
    /// this only mutates in-memory state.
    pub fn rotate(&mut self, kek: &[u8; KEY_LEN]) -> u32 {
        let next_id = self.entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        let mut raw = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut raw);
        let aad = dek_aad(next_id);
        let wrapped = encrypt(kek, &aad, &raw);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.entries.push(DekEntry {
            id: next_id,
            created_at_unix_ms: now_ms,
            wrapped_dek: wrapped,
            raw_dek: raw,
        });
        self.current_id = next_id;
        next_id
    }

    /// Load from `path`, unwrap every DEK under `kek`. Fails if the
    /// file is malformed, if any DEK fails AEAD verify (wrong KEK,
    /// tampered file), or if `current_id` doesn't match any entry.
    ///
    /// If `path` doesn't exist, bootstrap a fresh single-DEK
    /// keyring using `bootstrap_dek` (typically the pre-P1.7
    /// identity-HKDF output for backward compat; or freshly-random
    /// on first prod boot). Persists the new keyring so subsequent
    /// boots take the loaded path.
    pub fn load_or_bootstrap(
        path: &Path,
        kek: &[u8; KEY_LEN],
        bootstrap_dek: [u8; KEY_LEN],
    ) -> io::Result<Self> {
        if !path.exists() {
            // Bootstrap: single DEK with id=1, wrap under KEK, persist.
            let aad = dek_aad(1);
            let wrapped = encrypt(kek, &aad, &bootstrap_dek);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entries = vec![DekEntry {
                id: 1,
                created_at_unix_ms: now_ms,
                wrapped_dek: wrapped,
                raw_dek: bootstrap_dek,
            }];
            let ring = Self {
                entries,
                current_id: 1,
            };
            ring.save(path)?;
            return Ok(ring);
        }
        let raw = fs::read(path)?;
        let decoded: KeyringOnDisk = serde_json_min::parse(&raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("keyring {} parse: {e}", path.display()),
            )
        })?;
        if decoded.format_version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "keyring format_version {}: only 1 supported",
                    decoded.format_version
                ),
            ));
        }
        let mut entries = Vec::with_capacity(decoded.keys.len());
        for k in decoded.keys {
            let wrapped = decode_hex_bytes(&k.wrapped_dek_hex).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("keyring key id={}: {e}", k.id),
                )
            })?;
            let aad = dek_aad(k.id);
            let raw = decrypt(kek, &aad, &wrapped).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "keyring key id={} unwrap failed (wrong KEK?): {e}",
                        k.id
                    ),
                )
            })?;
            if raw.len() != KEY_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("keyring key id={}: DEK is {} bytes, expected {KEY_LEN}", k.id, raw.len()),
                ));
            }
            let mut dek = [0u8; KEY_LEN];
            dek.copy_from_slice(&raw);
            entries.push(DekEntry {
                id: k.id,
                created_at_unix_ms: k.created_at_unix_ms,
                wrapped_dek: wrapped,
                raw_dek: dek,
            });
        }
        if !entries.iter().any(|e| e.id == decoded.current_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "keyring current_id={} not found among {} entries",
                    decoded.current_id,
                    entries.len()
                ),
            ));
        }
        Ok(Self {
            entries,
            current_id: decoded.current_id,
        })
    }

    /// Serialise + atomically rename to `path`. Called on rotate.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let disk = KeyringOnDisk {
            format_version: 1,
            current_id: self.current_id,
            keys: self
                .entries
                .iter()
                .map(|e| KeyringKeyOnDisk {
                    id: e.id,
                    created_at_unix_ms: e.created_at_unix_ms,
                    wrapped_dek_hex: encode_hex_bytes(&e.wrapped_dek),
                })
                .collect(),
        };
        let json = serde_json_min::to_string(&disk);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Boot-log summary: `(current_id, key_count, oldest_key_age_ms)`.
    pub fn summary(&self) -> (u32, usize, u64) {
        let oldest = self
            .entries
            .iter()
            .map(|e| e.created_at_unix_ms)
            .min()
            .unwrap_or(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let age = now_ms.saturating_sub(oldest);
        (self.current_id, self.entries.len(), age)
    }
}

// impl Debug required for `unwrap_err().to_string()` in tests.
impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("entries", &self.entries.len())
            .field("current_id", &self.current_id)
            .finish()
    }
}

/// Fixed AAD for wrapping DEK id `n` — binds a wrapped DEK to its
/// declared id so a swap of `id`s at the JSON layer trips unwrap.
fn dek_aad(id: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(b"holofs-dek-v1");
    v.extend_from_slice(&id.to_be_bytes());
    v
}

/// Minimal serde-free JSON (de)ser for the keyring. Pulled out into
/// its own tiny module rather than adding `serde_json` to
/// holofs-storage's dep graph — the shape is fixed at 4 fields per
/// key + 3 at the top level.
mod serde_json_min {
    use super::*;

    #[derive(Debug)]
    pub(super) struct Parsed {
        pub format_version: u32,
        pub current_id: u32,
        pub keys: Vec<KeyRow>,
    }

    #[derive(Debug)]
    pub(super) struct KeyRow {
        pub id: u32,
        pub created_at_unix_ms: u64,
        pub wrapped_dek_hex: String,
    }

    pub(super) fn parse(bytes: &[u8]) -> Result<super::KeyringOnDisk, String> {
        // Delegate to a full JSON parser via holofs-model's existing
        // handrolled one? No — simpler to just do a small typed pass
        // here. Requires the input to be pretty-printed by `to_string`
        // below, so we don't try to handle every valid JSON shape.
        let text = std::str::from_utf8(bytes).map_err(|e| format!("not utf-8: {e}"))?;
        let mut format_version = 0u32;
        let mut current_id = 0u32;
        let mut keys: Vec<super::KeyringKeyOnDisk> = Vec::new();
        // Find the top-level fields via naive substring scans. Keys
        // list is parsed as an array of `{ id, created_at_unix_ms,
        // wrapped_dek_hex }` objects.
        for line in text.lines() {
            let t = line.trim().trim_end_matches(',');
            if let Some(v) = t.strip_prefix("\"format_version\":") {
                format_version = v.trim().parse().map_err(|e| format!("format_version: {e}"))?;
            } else if let Some(v) = t.strip_prefix("\"current_id\":") {
                current_id = v.trim().parse().map_err(|e| format!("current_id: {e}"))?;
            }
        }
        // Extract each key object: everything between `{` and `}` inside `"keys": [ … ]`.
        let keys_start = text.find("\"keys\":").ok_or_else(|| "missing keys field".to_string())?;
        let after = &text[keys_start..];
        let arr_start = after.find('[').ok_or_else(|| "keys: no [".to_string())?;
        let arr_end = after.rfind(']').ok_or_else(|| "keys: no ]".to_string())?;
        let inside = &after[arr_start + 1..arr_end];
        // Split on `}` boundaries; each chunk is one key object.
        let mut cursor = 0usize;
        while let Some(rel) = inside[cursor..].find('}') {
            let obj = &inside[cursor..cursor + rel + 1];
            if let Some(k) = parse_key_object(obj)? {
                keys.push(k);
            }
            cursor += rel + 1;
        }
        Ok(super::KeyringOnDisk {
            format_version,
            current_id,
            keys,
        })
    }

    fn parse_key_object(chunk: &str) -> Result<Option<super::KeyringKeyOnDisk>, String> {
        let open = match chunk.find('{') {
            Some(i) => i,
            None => return Ok(None),
        };
        let body = &chunk[open + 1..chunk.len() - 1];
        let mut id = 0u32;
        let mut created_at_unix_ms = 0u64;
        let mut wrapped_dek_hex = String::new();
        for field in body.split(',') {
            let t = field.trim();
            if let Some(v) = t.strip_prefix("\"id\":") {
                id = v.trim().parse().map_err(|e| format!("key.id: {e}"))?;
            } else if let Some(v) = t.strip_prefix("\"created_at_unix_ms\":") {
                created_at_unix_ms = v.trim().parse().map_err(|e| format!("key.created: {e}"))?;
            } else if let Some(v) = t.strip_prefix("\"wrapped_dek_hex\":") {
                let s = v.trim().trim_matches('"');
                wrapped_dek_hex = s.to_string();
            }
        }
        Ok(Some(super::KeyringKeyOnDisk {
            id,
            created_at_unix_ms,
            wrapped_dek_hex,
        }))
    }

    pub(super) fn to_string(disk: &super::KeyringOnDisk) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!(
            "  \"format_version\": {},\n",
            disk.format_version
        ));
        out.push_str(&format!("  \"current_id\": {},\n", disk.current_id));
        out.push_str("  \"keys\": [\n");
        for (i, k) in disk.keys.iter().enumerate() {
            out.push_str("    {");
            out.push_str(&format!(
                "\"id\": {}, \"created_at_unix_ms\": {}, \"wrapped_dek_hex\": \"{}\"",
                k.id, k.created_at_unix_ms, k.wrapped_dek_hex
            ));
            out.push_str("}");
            if i + 1 < disk.keys.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n");
        out.push_str("}\n");
        out
    }

    #[allow(dead_code)]
    fn _drop(_p: Parsed, _k: KeyRow) {}
}

/// Struct mirror of what lands on disk. Kept `pub(super)` so the
/// tiny JSON module can construct it.
pub(super) struct KeyringOnDisk {
    pub(super) format_version: u32,
    pub(super) current_id: u32,
    pub(super) keys: Vec<KeyringKeyOnDisk>,
}

pub(super) struct KeyringKeyOnDisk {
    pub(super) id: u32,
    pub(super) created_at_unix_ms: u64,
    pub(super) wrapped_dek_hex: String,
}

/// Hex helpers — kept local to avoid pulling `hex` into the dep
/// graph. Two-nibble lowercase / mixed-case tolerant on decode.

fn encode_hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn decode_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd hex length {}", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16)
            .map_err(|e| format!("bad hex byte at offset {i}: {e}"))?;
        out.push(byte);
    }
    Ok(out)
}

fn decode_hex_key(s: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = decode_hex_bytes(s)?;
    if bytes.len() != KEY_LEN {
        return Err(format!("expected {} bytes ({} hex chars), got {}", KEY_LEN, KEY_LEN * 2, bytes.len()));
    }
    let mut k = [0u8; KEY_LEN];
    k.copy_from_slice(&bytes);
    Ok(k)
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

    // === P1.7 envelope encryption ================================

    #[test]
    fn kek_source_identity_matches_legacy_derive() {
        let seed = [0xEEu8; 32];
        let src = KekSource::IdentitySeed;
        assert_eq!(src.load(&seed).unwrap(), derive_shard_key(&seed));
        assert_eq!(src.label(), "identity");
    }

    #[test]
    fn kek_source_file_reads_32_bytes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), [0x55u8; KEY_LEN]).unwrap();
        let src = KekSource::File(tmp.path().to_path_buf());
        let k = src.load(&[]).unwrap();
        assert_eq!(k, [0x55u8; KEY_LEN]);
    }

    #[test]
    fn kek_source_file_rejects_wrong_length() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), [0x00u8; 16]).unwrap(); // too short
        let src = KekSource::File(tmp.path().to_path_buf());
        let err = src.load(&[]).unwrap_err();
        assert!(err.to_string().contains("bytes, expected 32"));
    }

    #[test]
    fn kek_source_env_hex_roundtrips() {
        let hex_str = "ff".repeat(KEY_LEN);
        let src = KekSource::EnvHex(hex_str);
        let k = src.load(&[]).unwrap();
        assert_eq!(k, [0xFFu8; KEY_LEN]);
    }

    #[test]
    fn keyring_bootstraps_from_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("keyring.json");
        let kek = [0x11u8; KEY_LEN];
        let boot = derive_shard_key(&[0x99u8; 32]);
        let ring = Keyring::load_or_bootstrap(&path, &kek, boot).unwrap();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.current_id(), 1);
        assert!(path.exists(), "keyring should have been persisted");
        // Reopen — same state.
        let ring2 = Keyring::load_or_bootstrap(&path, &kek, [0; KEY_LEN]).unwrap();
        assert_eq!(ring2.current_id(), 1);
        assert_eq!(ring2.entries[0].raw_dek, boot);
    }

    #[test]
    fn keyring_encrypt_current_decrypts_via_decrypt_any() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ring =
            Keyring::load_or_bootstrap(&tmp.path().join("kr.json"), &[0x11u8; KEY_LEN], [0x22u8; KEY_LEN])
                .unwrap();
        let sealed = ring.encrypt_current(b"aad", b"payload bytes");
        let back = ring.decrypt_any(b"aad", &sealed).unwrap();
        assert_eq!(back, b"payload bytes");
    }

    #[test]
    fn keyring_rotation_keeps_old_shards_readable() {
        // The whole point of envelope: rotate the DEK, existing
        // ciphertext still decrypts under the old DEK because it's
        // retained in the ring.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("kr.json");
        let kek = [0x11u8; KEY_LEN];
        let mut ring = Keyring::load_or_bootstrap(&path, &kek, [0x22u8; KEY_LEN]).unwrap();

        let old_sealed = ring.encrypt_current(b"aad", b"pre-rotate");
        let old_id = ring.current_id();

        ring.rotate(&kek);
        assert_eq!(ring.len(), 2);
        assert_ne!(ring.current_id(), old_id, "rotate must bump current_id");

        let new_sealed = ring.encrypt_current(b"aad", b"post-rotate");
        // Old-sealed decrypts under the old DEK (via try-each-key).
        assert_eq!(ring.decrypt_any(b"aad", &old_sealed).unwrap(), b"pre-rotate");
        // New-sealed decrypts under the new DEK.
        assert_eq!(ring.decrypt_any(b"aad", &new_sealed).unwrap(), b"post-rotate");

        // Persist + reload — both still work.
        ring.save(&path).unwrap();
        let ring2 = Keyring::load_or_bootstrap(&path, &kek, [0u8; KEY_LEN]).unwrap();
        assert_eq!(ring2.len(), 2);
        assert_eq!(ring2.decrypt_any(b"aad", &old_sealed).unwrap(), b"pre-rotate");
        assert_eq!(ring2.decrypt_any(b"aad", &new_sealed).unwrap(), b"post-rotate");
    }

    #[test]
    fn keyring_load_with_wrong_kek_fails_at_unwrap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("kr.json");
        let kek_good = [0x11u8; KEY_LEN];
        let kek_bad = [0x22u8; KEY_LEN];
        Keyring::load_or_bootstrap(&path, &kek_good, [0x33u8; KEY_LEN]).unwrap();
        let err = Keyring::load_or_bootstrap(&path, &kek_bad, [0u8; KEY_LEN]).unwrap_err();
        assert!(err.to_string().contains("unwrap failed"), "got: {err}");
    }
}
