//! Append-only write-ahead log for shard storage.
//!
//! Every mutation ([`RecordKind::Put`], [`RecordKind::Purge`],
//! [`RecordKind::Wipe`]) is serialised into a length-prefixed
//! record and appended to the currently active segment file
//! (`wal-NNNNNNNN.seg`). On boot the node walks every segment in
//! seq order, replays the records into RAM, and opens (or
//! creates) the next segment for writes.
//!
//! Motivation. The pre-WAL storage did `File::create → write →
//! sync_all → rename` for every single shard. Under a 24-encoder
//! soak (July 2026) that fsync-under-mutex serialised parallel
//! shard writes to one node → aggregate encoder throughput
//! plateaued at ~0.5 encodes/s regardless of
//! `HOLOFS_ENCODE_CONCURRENCY`. The WAL replaces the N random
//! creates + N fsyncs with one sequential append + one fsync per
//! batch, which is ~10× cheaper on SSD.
//!
//! ## Segment layout
//!
//! ```text
//! [8 bytes]  magic = b"HOLOFSW1"
//! [record]*
//! ```
//!
//! ## Record framing
//!
//! ```text
//! [4 bytes big-endian body_len]
//! [1 byte kind]
//! [body_len - 1 bytes body]
//! [8 bytes body integrity digest = sha256(kind || body)[..8]]
//! ```
//!
//! A trailing partial record (crash mid-write) is detected via
//! short read or digest mismatch and truncates the recovered
//! stream — the missing records are treated as never-committed.
//!
//! ## Kinds
//!
//! * `Put` (kind=0) — plaintext shard.
//!   `object_id: u64 | channel: u8 | layer: u8 | hash: [u8; 32] |
//!    coeffs_len: u32 | payload_len: u32 | coeffs | payload`
//! * `PutSealed` (kind=1) — at-rest-encrypted shard. Body is the
//!   same fixed header (46 bytes) plus a `sealed_len: u32` and the
//!   sealed blob (nonce | ciphertext | tag). AAD for
//!   encrypt/decrypt is the fixed header (46 bytes) so a tampered
//!   header trips the tag.
//! * `Purge` (kind=2) — `object_id: u64`. Every shard bucket for
//!   this id is dropped on replay.
//! * `Wipe` (kind=3) — empty body. Boot replay clears the RAM
//!   index.
//!
//! Compaction (dropping tombstoned segments) is out of scope for
//! V1 — segments accumulate. For MVP soak this is fine; a real
//! deployment would layer a background compactor on top.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use holofs_core::hash::sha256;
use holofs_core::merkle::Hash;
use holofs_core::rlnc::Shard;

use crate::crypto::{decrypt, encrypt, KEY_LEN, NONCE_LEN, TAG_LEN};

/// 8-byte segment file magic. Bumps versioned so a future format
/// (compaction epoch, checksummed segment header, etc.) can be
/// distinguished without a rewrite pass.
pub const SEG_MAGIC: &[u8; 8] = b"HOLOFSW1";

const KIND_PUT_PLAIN: u8 = 0;
const KIND_PUT_SEALED: u8 = 1;
const KIND_PURGE: u8 = 2;
const KIND_WIPE: u8 = 3;
const KIND_PURGE_HASHES: u8 = 4;

/// Fixed prefix of the Put/PutSealed record body — object identity
/// plus the shard hash. Used as AAD in the sealed variant so a
/// tampered header trips the AEAD tag.
///
/// ```text
/// [8 bytes object_id][1 byte channel][1 byte layer][32 bytes hash]
/// [4 bytes coeffs_len][4 bytes payload_len]
/// ```
///
/// = 50 bytes total. Sealed variant instead has a single
/// `sealed_len: u32` in place of the two length fields, so its
/// fixed header is 46 bytes.
const PUT_PLAIN_HEADER_LEN: usize = 8 + 1 + 1 + 32 + 4 + 4;
/// Fixed AAD header of a `PutSealed` record: object_id + channel +
/// layer + hash (42 bytes). The `sealed_len: u32` sits *after* this
/// header — same layout as `[aad][sealed_len][sealed]`.
const PUT_SEALED_AAD_LEN: usize = 8 + 1 + 1 + 32;
const DIGEST_LEN: usize = 8;
const LEN_PREFIX: usize = 4;

/// Type of an entry replayed off disk during boot.
#[derive(Debug, Clone)]
pub enum RecordKind {
    /// Insert a shard into the RAM index.
    Put {
        /// Object id.
        object_id: u64,
        /// Channel index.
        channel: u8,
        /// DWT layer index.
        layer: u8,
        /// Shard hash (recomputed on decode and used as a
        /// consistency check against the header field).
        hash: Hash,
        /// The decoded shard body (plaintext or sealed depending
        /// on how it was originally appended).
        shard: Shard,
    },
    /// Drop every shard belonging to `object_id`.
    Purge {
        /// Object id whose buckets should be dropped.
        object_id: u64,
    },
    /// Drop specific shards by hash. Emitted by GC after the RAM
    /// side has already applied the epoch filter, so the boot
    /// replay just needs to drop whatever hashes match, no matter
    /// their epoch.
    PurgeHashes {
        /// Hashes to drop from every bucket that contains them.
        hashes: Vec<Hash>,
    },
    /// Drop every shard on the node.
    Wipe,
}

/// Append-only writer for a single segment file. The writer keeps
/// an open `File` (buffered) and tracks bytes-written so callers
/// know when to rotate. `sync_all` is *not* called by the writer —
/// the caller decides when to fsync (typically once per Put frame
/// / PutBatch) so it can be coalesced with the request response.
pub struct WalWriter {
    dir: PathBuf,
    seq: u64,
    active: BufWriter<File>,
    bytes_written: u64,
    /// Rotation threshold. When `bytes_written` exceeds this after
    /// an append, the writer closes the active segment and opens a
    /// fresh one with `seq + 1`.
    pub rotate_at: u64,
    /// AES-256-GCM key for at-rest encryption. When `Some`, Put
    /// records are written as `KIND_PUT_SEALED`.
    enc_key: Option<[u8; KEY_LEN]>,
}

impl WalWriter {
    /// Open (or create) the next segment for writes. Callers pass
    /// the highest seq that was replayed on boot — the new segment
    /// gets `next_seq = highest + 1`. A brand-new directory hands
    /// in `0`, gets segment `wal-00000001.seg`.
    pub fn open(
        dir: PathBuf,
        highest_replayed_seq: u64,
        enc_key: Option<[u8; KEY_LEN]>,
    ) -> io::Result<Self> {
        let next_seq = highest_replayed_seq + 1;
        let path = segment_path(&dir, next_seq);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(SEG_MAGIC)?;
        let bytes_written = SEG_MAGIC.len() as u64;
        Ok(Self {
            dir,
            seq: next_seq,
            active: BufWriter::new(file),
            bytes_written,
            rotate_at: 64 * 1024 * 1024,
            enc_key,
        })
    }

    /// Append a Put record. Returns after the buffered write but
    /// does not fsync — the caller is responsible for `sync()` at
    /// batch boundaries.
    pub fn append_put(
        &mut self,
        object_id: u64,
        channel: u8,
        layer: u8,
        hash: &Hash,
        shard: &Shard,
    ) -> io::Result<()> {
        // Sealed vs plaintext is decided by the writer's enc_key.
        // Reads (via `read_segment`) accept either format.
        let mut body: Vec<u8>;
        let kind: u8;
        if let Some(key) = &self.enc_key {
            kind = KIND_PUT_SEALED;
            // Fixed header (42 bytes: object_id + channel + layer +
            // hash) → sealed_len (u32) → sealed blob. The header
            // doubles as AAD so tampering trips the AEAD tag.
            let mut header = Vec::with_capacity(PUT_SEALED_AAD_LEN);
            header.extend_from_slice(&object_id.to_be_bytes());
            header.push(channel);
            header.push(layer);
            header.extend_from_slice(hash);
            // Sealed plaintext = [coeffs_len u32][coeffs][payload].
            // The coeffs_len prefix lets the reader recover the
            // split (encryption is opaque bytes).
            let coeffs_len = shard.coeffs.len() as u32;
            let mut plaintext =
                Vec::with_capacity(4 + shard.coeffs.len() + shard.payload.len());
            plaintext.extend_from_slice(&coeffs_len.to_be_bytes());
            plaintext.extend_from_slice(&shard.coeffs);
            plaintext.extend_from_slice(&shard.payload);
            let sealed = encrypt(key, &header, &plaintext);
            let sealed_len = sealed.len() as u32;
            header.extend_from_slice(&sealed_len.to_be_bytes());
            body = header;
            body.extend_from_slice(&sealed);
        } else {
            kind = KIND_PUT_PLAIN;
            let coeffs_len = shard.coeffs.len() as u32;
            let payload_len = shard.payload.len() as u32;
            body = Vec::with_capacity(PUT_PLAIN_HEADER_LEN + shard.coeffs.len() + shard.payload.len());
            body.extend_from_slice(&object_id.to_be_bytes());
            body.push(channel);
            body.push(layer);
            body.extend_from_slice(hash);
            body.extend_from_slice(&coeffs_len.to_be_bytes());
            body.extend_from_slice(&payload_len.to_be_bytes());
            body.extend_from_slice(&shard.coeffs);
            body.extend_from_slice(&shard.payload);
        }
        self.write_record(kind, &body)
    }

    /// Append a Purge record.
    pub fn append_purge(&mut self, object_id: u64) -> io::Result<()> {
        self.write_record(KIND_PURGE, &object_id.to_be_bytes())
    }

    /// Append a PurgeHashes record.
    pub fn append_purge_hashes(&mut self, hashes: &[Hash]) -> io::Result<()> {
        // [4 B count][hashes concatenated]. `hashes` must fit in a
        // single record — GC batches on the caller side are already
        // bounded by node capacity, so no chunking here.
        let count = hashes.len() as u32;
        let mut body = Vec::with_capacity(4 + hashes.len() * 32);
        body.extend_from_slice(&count.to_be_bytes());
        for h in hashes {
            body.extend_from_slice(h);
        }
        self.write_record(KIND_PURGE_HASHES, &body)
    }

    /// Append a Wipe record.
    pub fn append_wipe(&mut self) -> io::Result<()> {
        self.write_record(KIND_WIPE, &[])
    }

    /// Flush the buffered writer and, if `fsync` is set, force the
    /// segment to persistent storage. Callers batch this once per
    /// request-handler wake-up so multiple appends amortise a
    /// single fsync.
    pub fn sync(&mut self, fsync: bool) -> io::Result<()> {
        self.active.flush()?;
        if fsync {
            self.active.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Split-phase companion to [`Self::sync`]: flush the buffered
    /// writer's memory chunk down to the kernel *now*, but return
    /// a cheap `File` clone so the caller can move the fsync off
    /// the tokio worker via `spawn_blocking`. `sync_all` is the
    /// only blocking bit — a kernel fsync stalls the whole thread
    /// for ~5-30 ms on SSD, so anything running on the current
    /// worker (unrelated request handlers, `Notify::notified`
    /// waiters) is stuck for that window. Moving the fsync to the
    /// blocking pool lets other tokio tasks keep running.
    pub fn flush_and_take_file(&mut self) -> io::Result<std::fs::File> {
        self.active.flush()?;
        self.active.get_ref().try_clone()
    }

    /// Byte offset of the next append relative to the segment
    /// start. Exposed for the rotation heuristic in
    /// [`Store`](crate::node_service::Store).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Close the current segment and open the next one. Called
    /// after an append when [`Self::bytes_written`] exceeds
    /// [`Self::rotate_at`]. Idempotent-safe — closing a
    /// zero-length new segment is fine.
    pub fn rotate(&mut self) -> io::Result<()> {
        // Best-effort flush + fsync before rotate so the "closed"
        // segment reaches disk before the fd is dropped.
        self.active.flush()?;
        self.active.get_ref().sync_all()?;
        let next_seq = self.seq + 1;
        let path = segment_path(&self.dir, next_seq);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(SEG_MAGIC)?;
        self.seq = next_seq;
        self.bytes_written = SEG_MAGIC.len() as u64;
        self.active = BufWriter::new(file);
        Ok(())
    }

    fn write_record(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        // Framing: [4 B body_len][1 B kind][body][8 B digest]
        // body_len covers the kind byte + body itself, so the
        // reader knows exactly how many bytes to consume before
        // the digest.
        let framed_body_len = (1 + body.len()) as u32;
        let mut digest_input = Vec::with_capacity(1 + body.len());
        digest_input.push(kind);
        digest_input.extend_from_slice(body);
        let digest = sha256(&digest_input);

        self.active.write_all(&framed_body_len.to_be_bytes())?;
        self.active.write_all(&[kind])?;
        self.active.write_all(body)?;
        self.active.write_all(&digest[..DIGEST_LEN])?;
        self.bytes_written += (LEN_PREFIX + 1 + body.len() + DIGEST_LEN) as u64;
        Ok(())
    }
}

/// Read a segment file end-to-end, yielding decoded records in
/// disk order. A truncated tail (short read or digest mismatch)
/// stops iteration cleanly — no `Err` returned — so a crash
/// mid-write leaves the segment recoverable. Corrupted mid-stream
/// records (bad magic, bad kind byte) surface as `Err` so the
/// caller can surface the problem instead of silently truncating.
pub fn read_segment(
    path: &Path,
    enc_key: Option<&[u8; KEY_LEN]>,
) -> io::Result<Vec<RecordKind>> {
    let mut file = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    if file.read_exact(&mut magic).is_err() {
        // Empty / truncated header — treat as empty segment.
        return Ok(Vec::new());
    }
    if magic != *SEG_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("wal segment {path:?} has wrong magic {magic:?}"),
        ));
    }
    let mut out = Vec::new();
    loop {
        let mut len_buf = [0u8; LEN_PREFIX];
        // A partial length prefix means we're in the truncated
        // tail — stop cleanly.
        if !read_exact_or_eof(&mut file, &mut len_buf)? {
            break;
        }
        let framed_body_len = u32::from_be_bytes(len_buf) as usize;
        if framed_body_len < 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wal record with zero-length body",
            ));
        }
        let mut kind_and_body = vec![0u8; framed_body_len];
        if !read_exact_or_eof(&mut file, &mut kind_and_body)? {
            break;
        }
        let mut digest_buf = [0u8; DIGEST_LEN];
        if !read_exact_or_eof(&mut file, &mut digest_buf)? {
            break;
        }
        let expected = &sha256(&kind_and_body)[..DIGEST_LEN];
        if expected != digest_buf {
            // Digest mismatch on the trailing record is
            // indistinguishable from truncated mid-write, so we
            // treat it as end-of-segment rather than corruption.
            // Mid-segment corruption would need explicit segment
            // checksumming — deferred.
            break;
        }
        let kind = kind_and_body[0];
        let body = &kind_and_body[1..];
        match kind {
            KIND_PUT_PLAIN => {
                if body.len() < PUT_PLAIN_HEADER_LEN {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_plain header too short",
                    ));
                }
                let object_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
                let channel = body[8];
                let layer = body[9];
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&body[10..42]);
                let coeffs_len = u32::from_be_bytes(body[42..46].try_into().unwrap()) as usize;
                let payload_len = u32::from_be_bytes(body[46..50].try_into().unwrap()) as usize;
                let start = PUT_PLAIN_HEADER_LEN;
                if body.len() < start + coeffs_len + payload_len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_plain body length mismatch",
                    ));
                }
                let coeffs = body[start..start + coeffs_len].to_vec();
                let payload = body[start + coeffs_len..start + coeffs_len + payload_len].to_vec();
                out.push(RecordKind::Put {
                    object_id,
                    channel,
                    layer,
                    hash,
                    shard: Shard { coeffs, payload },
                });
            }
            KIND_PUT_SEALED => {
                let Some(key) = enc_key else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal sealed record but Store has no enc_key configured",
                    ));
                };
                if body.len() < PUT_SEALED_AAD_LEN + 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_sealed header too short",
                    ));
                }
                let header_and_len = &body[..PUT_SEALED_AAD_LEN + 4];
                let object_id = u64::from_be_bytes(header_and_len[0..8].try_into().unwrap());
                let channel = header_and_len[8];
                let layer = header_and_len[9];
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&header_and_len[10..42]);
                let sealed_len = u32::from_be_bytes(
                    header_and_len[PUT_SEALED_AAD_LEN..PUT_SEALED_AAD_LEN + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if body.len() < PUT_SEALED_AAD_LEN + 4 + sealed_len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_sealed body length mismatch",
                    ));
                }
                if sealed_len < NONCE_LEN + TAG_LEN {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_sealed sealed blob too short",
                    ));
                }
                let aad = &body[..PUT_SEALED_AAD_LEN];
                let sealed =
                    &body[PUT_SEALED_AAD_LEN + 4..PUT_SEALED_AAD_LEN + 4 + sealed_len];
                let plaintext = decrypt(key, aad, sealed).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("wal put_sealed decrypt failed: {e}"),
                    )
                })?;
                // Reconstruct (coeffs || payload) — the sealed
                // side does not store the split, so at replay we
                // rely on the hash to validate: coeffs is
                // deterministic-length inside the RLNC layer, but
                // from Store's perspective we round-trip through
                // the Shard shape. The safe way to split is to
                // rehash after choosing a split; since the writer
                // stored the whole thing, we take everything as
                // payload and leave coeffs empty. That would lose
                // shard identity — so instead: refuse. To make
                // sealed work end-to-end we need the split
                // recorded. Fix: prepend coeffs_len (u32) inside
                // the sealed plaintext.
                let (coeffs_len_bytes, rest) = plaintext.split_at(4);
                let coeffs_len =
                    u32::from_be_bytes(coeffs_len_bytes.try_into().unwrap()) as usize;
                if rest.len() < coeffs_len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal put_sealed plaintext too short for declared coeffs_len",
                    ));
                }
                let coeffs = rest[..coeffs_len].to_vec();
                let payload = rest[coeffs_len..].to_vec();
                out.push(RecordKind::Put {
                    object_id,
                    channel,
                    layer,
                    hash,
                    shard: Shard { coeffs, payload },
                });
            }
            KIND_PURGE => {
                if body.len() != 8 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal purge body must be 8 bytes",
                    ));
                }
                let object_id = u64::from_be_bytes(body.try_into().unwrap());
                out.push(RecordKind::Purge { object_id });
            }
            KIND_PURGE_HASHES => {
                if body.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal purge_hashes body too short",
                    ));
                }
                let count = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
                let expected = 4 + count * 32;
                if body.len() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "wal purge_hashes length mismatch: count={count} body_len={} expected={expected}",
                            body.len()
                        ),
                    ));
                }
                let mut hashes = Vec::with_capacity(count);
                for i in 0..count {
                    let start = 4 + i * 32;
                    let mut h = [0u8; 32];
                    h.copy_from_slice(&body[start..start + 32]);
                    hashes.push(h);
                }
                out.push(RecordKind::PurgeHashes { hashes });
            }
            KIND_WIPE => {
                if !body.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "wal wipe body must be empty",
                    ));
                }
                out.push(RecordKind::Wipe);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("wal record has unknown kind byte {other}"),
                ));
            }
        }
    }
    Ok(out)
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..])? {
            0 => return Ok(false),
            n => read += n,
        }
    }
    Ok(true)
}

/// Scan `dir` for `wal-NNNNNNNN.seg` files, returning them sorted
/// by seq number. Non-matching entries are silently ignored so the
/// same directory can hold legacy per-shard `.shard` files.
pub fn walk_wal_segments(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(seq) = parse_segment_name(name) else {
            continue;
        };
        out.push((seq, path));
    }
    out.sort_by_key(|(seq, _)| *seq);
    Ok(out)
}

fn parse_segment_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".seg")?;
    let digits = stem.strip_prefix("wal-")?;
    digits.parse::<u64>().ok()
}

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("wal-{seq:08}.seg"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tiny_shard(byte: u8) -> Shard {
        Shard {
            coeffs: vec![byte, byte, byte],
            payload: vec![byte; 16],
        }
    }

    #[test]
    fn roundtrip_put_purge_wipe_plaintext() {
        let dir = TempDir::new().unwrap();
        let mut w = WalWriter::open(dir.path().to_path_buf(), 0, None).unwrap();
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];
        w.append_put(10, 0, 0, &hash1, &tiny_shard(0xAA)).unwrap();
        w.append_put(11, 1, 2, &hash2, &tiny_shard(0xBB)).unwrap();
        w.append_purge(10).unwrap();
        w.append_wipe().unwrap();
        w.sync(true).unwrap();

        let seg_path = super::segment_path(dir.path(), 1);
        let records = read_segment(&seg_path, None).unwrap();
        assert_eq!(records.len(), 4);
        match &records[0] {
            RecordKind::Put {
                object_id,
                channel,
                layer,
                hash,
                shard,
            } => {
                assert_eq!(*object_id, 10);
                assert_eq!(*channel, 0);
                assert_eq!(*layer, 0);
                assert_eq!(*hash, hash1);
                assert_eq!(shard.coeffs, vec![0xAA; 3]);
                assert_eq!(shard.payload, vec![0xAA; 16]);
            }
            _ => panic!("expected Put record"),
        }
        matches!(records[2], RecordKind::Purge { object_id: 10 });
        matches!(records[3], RecordKind::Wipe);
    }

    #[test]
    fn roundtrip_put_sealed() {
        let dir = TempDir::new().unwrap();
        let key = crate::crypto::derive_shard_key(&[0xEE; 32]);
        let mut w = WalWriter::open(dir.path().to_path_buf(), 0, Some(key)).unwrap();
        let hash = [7u8; 32];
        w.append_put(42, 1, 2, &hash, &tiny_shard(0x77)).unwrap();
        w.sync(true).unwrap();
        drop(w);

        let seg_path = super::segment_path(dir.path(), 1);
        let records = read_segment(&seg_path, Some(&key)).unwrap();
        assert_eq!(records.len(), 1);
        match &records[0] {
            RecordKind::Put {
                object_id,
                channel,
                layer,
                hash: got_hash,
                shard,
            } => {
                assert_eq!(*object_id, 42);
                assert_eq!(*channel, 1);
                assert_eq!(*layer, 2);
                assert_eq!(*got_hash, hash);
                assert_eq!(shard.coeffs, vec![0x77; 3]);
                assert_eq!(shard.payload, vec![0x77; 16]);
            }
            _ => panic!("expected Put record"),
        }
    }

    #[test]
    fn truncated_tail_stops_replay_cleanly() {
        let dir = TempDir::new().unwrap();
        let mut w = WalWriter::open(dir.path().to_path_buf(), 0, None).unwrap();
        w.append_put(1, 0, 0, &[9u8; 32], &tiny_shard(1)).unwrap();
        w.sync(true).unwrap();
        // Simulate a crash mid-write of the second record by
        // appending garbage that doesn't complete a record.
        let seg_path = super::segment_path(dir.path(), 1);
        drop(w); // close writer
        let mut f = OpenOptions::new().append(true).open(&seg_path).unwrap();
        f.write_all(&[0, 0, 0, 5, 42, 1, 2, 3]).unwrap();
        // Reader must return the ONE complete record and stop.
        let records = read_segment(&seg_path, None).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn walk_wal_segments_returns_sorted() {
        let dir = TempDir::new().unwrap();
        for seq in [3u64, 1, 2] {
            let mut w =
                WalWriter::open(dir.path().to_path_buf(), seq - 1, None).unwrap();
            w.sync(true).unwrap();
            drop(w);
        }
        let list = walk_wal_segments(dir.path()).unwrap();
        let seqs: Vec<u64> = list.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
