//! Node service: tokio TCP, accepts wire frames (see `holofs-wire`) and holds
//! a shard store keyed by `(object_id, channel, layer)`.
//!
//! Two storage modes:
//! - **in-memory** (`Store::new`) — pure RAM `HashMap`. Fast, lost on process
//!   exit. Used by tests and in-process demos.
//! - **persistent** (`Store::open(dir)`) — RAM index + one file per shard in
//!   `dir`. Atomic writes via `.tmp` + rename. On startup it scans `dir` and
//!   rebuilds the index. Crash-safe w.r.t. individual mutations: either the
//!   shard file is fully present or it is absent.
//!
//! Shard file format (`HOLOFSS1`):
//! ```text
//! magic:        8  bytes = b"HOLOFSS1"
//! object_id:    8  bytes BE
//! channel:      1  byte
//! layer:        1  byte
//! coeffs_len:   4  bytes BE
//! payload_len:  4  bytes BE
//! coeffs:       coeffs_len bytes
//! payload:      payload_len bytes
//! ```
//! Filename = `hex(sha256(shard))` + `.shard`, located at
//! `dir/<first 2 hex>/<remaining 62 hex>.shard` (git-style fanout so that
//! `ls` does not choke on tens of thousands of files in one directory).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::identity::NodeIdentity;
use holofs_core::hash::hex;
use holofs_core::merkle::{shard_hash, Hash};
use holofs_core::rlnc::Shard;
use holofs_wire::{read_frame, write_frame, Request, Response};

/// Process-wide breakdown of the node-side PUT wall-time. Every completed
/// `Request::Put` / `Request::PutBatch` bumps three ns counters and one
/// count counter, so a caller can read a `(lock_wait, put_appended,
/// wal_wait)` triple. Cheap: three `fetch_add` per PUT — well below the
/// noise floor of any file-based tracing we tried. Read via the
/// `Request::PutTimings` wire frame (added specifically for the July 2026
/// fanout-amplification study).
pub static NODE_PUT_LOCK_WAIT_NS_SUM: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static NODE_PUT_APPEND_NS_SUM: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static NODE_PUT_WAL_WAIT_NS_SUM: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static NODE_PUT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

type Key = (u64, u8, u8);
/// Milliseconds since UNIX_EPOCH. Assigned by `Store::put` at
/// write-time (or reconstructed from filesystem mtime on
/// [`Store::open`]). Feeds the epoch-based GC pass — see
/// [`Store::current_epoch`] and [`Store::purge_by_hashes_up_to`].
pub type WriteEpoch = u64;

/// Current wall-clock as a [`WriteEpoch`]. Free-standing so callers
/// (gateway GC pass, tests) can snapshot the boundary without going
/// through a Store instance.
pub fn now_epoch() -> WriteEpoch {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const SHARD_MAGIC_V1: &[u8; 8] = b"HOLOFSS1";
/// same file layout as v1 but the `coeffs || payload` blob is
/// sealed with AES-256-GCM. See [`crate::crypto`] for the wire format.
const SHARD_MAGIC_V2: &[u8; 8] = b"HOLOFSS2";
/// Legacy alias kept for call sites (tests). New writers
/// pick the magic based on the store's `enc_key` field.
#[allow(dead_code)]
const SHARD_MAGIC: &[u8; 8] = SHARD_MAGIC_V1;

/// Store: for each (object_id, channel, layer) — `HashMap<shard_hash,
/// (Shard, WriteEpoch)>`. Using the hash as the key gives O(1) dedup;
/// the epoch () tags each entry with its wall-clock write time so
/// concurrent GC can protect fresh writes. A repeated PUT of the same
/// shard is a no-op *for the payload* but bumps the epoch to the
/// current time — the effect is "this shard is still live", so a GC
/// snapshot taken before the re-PUT can't purge it either.
///
/// If `dir` is set, every mutation (`put`/`purge`/`wipe`) is mirrored to disk.
pub struct Store {
    shards: HashMap<Key, HashMap<Hash, (Shard, WriteEpoch)>>,
    dir: Option<PathBuf>,
    /// Append-only log for persistent stores. Every mutation
    /// (`put` / `purge` / `wipe` / GC-`purge_by_hashes_up_to`)
    /// appends one record here; boot recovers the RAM index by
    /// replaying the segments (+ legacy `.shard` files for rolling
    /// upgrade). See [`crate::wal`] for the on-disk format.
    wal: Option<crate::wal::WalWriter>,
    /// Group-commit bookkeeping. Every append bumps
    /// `wal_next_seq`; a background flusher periodically calls
    /// `wal.sync()` and stores the flushed seq in
    /// `wal_synced_seq`, then wakes any handler waiting for their
    /// seq to become durable. Turns N concurrent Put frames into
    /// one shared fsync — the key win of WAL under the soak
    /// workload where the client sends `Request::Put` per shard,
    /// not `Request::PutBatch`.
    wal_next_seq: Arc<std::sync::atomic::AtomicU64>,
    wal_synced_seq: Arc<std::sync::atomic::AtomicU64>,
    wal_notify: Arc<tokio::sync::Notify>,
    /// per-node AES-256-GCM key derived from
    /// `NodeIdentity::to_bytes()` via HKDF-SHA256. `None` = plaintext
    /// shard files on disk. Reads accept both formats regardless of
    /// this field so upgrades roll gracefully. Writes pick the
    /// format based on this being `Some(_)`.
    enc_key: Option<[u8; crate::crypto::KEY_LEN]>,
    /// Whether `write_shard_file` calls `sync_all()` before rename.
    /// Default `true` (safe): each Put waits for the kernel to flush
    /// the shard payload to persistent storage before Ack. Setting
    /// this off (via `HOLOFS_NODE_FSYNC=0`) trades that per-shard
    /// durability barrier for ~13× encoder throughput — measured in
    /// the July 2026 soak, where fsync-under-mutex was serialising
    /// every concurrent shard Put on one node. RLNC replication
    /// across N nodes tolerates the loss of a few unsynced shards
    /// on a single crash, so the trade is acceptable for most
    /// workloads; production deployments with stricter durability
    /// SLAs should keep the default.
    sync_on_write: bool,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            shards: HashMap::new(),
            dir: None,
            wal: None,
            wal_next_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_synced_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_notify: Arc::new(tokio::sync::Notify::new()),
            enc_key: None,
            sync_on_write: true,
        }
    }
}

impl Store {
    /// In-memory store. Process restart = all shards lost.
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the per-shard `sync_all()` on writes. See
    /// [`Self::sync_on_write`] for the durability / throughput
    /// trade-off. Callers plumb this from `HOLOFS_NODE_FSYNC`.
    pub fn set_sync_on_write(&mut self, on: bool) {
        self.sync_on_write = on;
    }

    /// Persistent store: index in RAM, shard files in `dir`. On startup it
    /// scans `dir` and rebuilds the index from existing files. `dir` is
    /// created if it does not exist.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_inner(dir, None)
    }

    /// Persistent store with an AES-256-GCM key for at-rest encryption.
    /// The key is what [`crate::crypto::derive_shard_key`] returns when
    /// fed the node's [`crate::identity::NodeIdentity::to_bytes`] seed.
    /// New writes land as `HOLOFSS2` sealed files; reads accept both
    /// `HOLOFSS1` (legacy plaintext) and `HOLOFSS2` (sealed) transparently
    /// so upgrades roll without a rewrite pass.
    pub fn open_with_key(
        dir: impl AsRef<Path>,
        key: [u8; crate::crypto::KEY_LEN],
    ) -> io::Result<Self> {
        Self::open_inner(dir, Some(key))
    }

    fn open_inner(
        dir: impl AsRef<Path>,
        enc_key: Option<[u8; crate::crypto::KEY_LEN]>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut shards: HashMap<Key, HashMap<Hash, (Shard, WriteEpoch)>> = HashMap::new();
        // 1. Legacy per-shard files (rolling upgrade). Skipping
        //    silently is fine — a WAL-only directory just has none.
        for entry in walk_shard_files(&dir)? {
            let epoch = fs::metadata(&entry)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            match read_shard_file(&entry, enc_key.as_ref()) {
                Ok((k, h, shard)) => {
                    shards.entry(k).or_default().insert(h, (shard, epoch));
                }
                Err(e) => {
                    eprintln!("Store::open: skipping broken file {entry:?}: {e}");
                }
            }
        }
        // 2. WAL segments, in seq order. Records apply on top of
        //    whatever the legacy scan already produced — Put
        //    overwrites, Purge/PurgeHashes/Wipe drop.
        let segments = crate::wal::walk_wal_segments(&dir)?;
        let mut highest_seq: u64 = 0;
        for (seq, path) in &segments {
            highest_seq = (*seq).max(highest_seq);
            let epoch_from_mtime = fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let records = crate::wal::read_segment(path, enc_key.as_ref())?;
            for rec in records {
                match rec {
                    crate::wal::RecordKind::Put {
                        object_id,
                        channel,
                        layer,
                        hash,
                        shard,
                    } => {
                        shards
                            .entry((object_id, channel, layer))
                            .or_default()
                            .insert(hash, (shard, epoch_from_mtime));
                    }
                    crate::wal::RecordKind::Purge { object_id } => {
                        shards.retain(|(o, _, _), _| *o != object_id);
                    }
                    crate::wal::RecordKind::PurgeHashes { hashes } => {
                        use std::collections::HashSet;
                        let set: HashSet<Hash> = hashes.into_iter().collect();
                        shards.retain(|_, bucket| {
                            bucket.retain(|h, _| !set.contains(h));
                            !bucket.is_empty()
                        });
                    }
                    crate::wal::RecordKind::Wipe => {
                        shards.clear();
                    }
                }
            }
        }
        // 3. Open the next segment for writes. Even a brand-new
        //    directory gets seq=1 so writes never share a file with
        //    a replayed segment.
        let wal = crate::wal::WalWriter::open(dir.clone(), highest_seq, enc_key)?;
        Ok(Store {
            shards,
            dir: Some(dir),
            wal: Some(wal),
            wal_next_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_synced_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_notify: Arc::new(tokio::sync::Notify::new()),
            enc_key,
            sync_on_write: true,
        })
    }

    /// Clone the three group-commit handles so the flusher task can
    /// live outside the Store. `wal_next_seq` is bumped on every
    /// append; the flusher periodically fsyncs and publishes the
    /// covered seq to `wal_synced_seq`, then notifies waiters.
    pub fn group_commit_handles(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<tokio::sync::Notify>,
    ) {
        (
            Arc::clone(&self.wal_next_seq),
            Arc::clone(&self.wal_synced_seq),
            Arc::clone(&self.wal_notify),
        )
    }

    /// Returns `true` if the shard was actually added (false →
    /// duplicate). Either way the entry's write-epoch is bumped to
    /// [`now_epoch`]: a re-PUT of an existing hash is a "still-live"
    /// signal, and bumping keeps a concurrent GC snapshot from
    /// purging it just because its original epoch predated the
    /// snapshot.
    pub fn put(&mut self, k: Key, shard: Shard) -> bool {
        let (ok, _seq) = self.put_appended(k, shard);
        // No flusher wired → do a sync inline so single-Put callers
        // (test paths and legacy PutBatch handling) still get
        // durability.
        if ok {
            let sync_on = self.sync_on_write;
            if let Some(wal) = &mut self.wal {
                let _ = wal.sync(sync_on);
            }
            self.maybe_rotate_wal();
        }
        ok
    }

    /// Append-only companion to [`Self::put`]. Returns `(ok, seq)`
    /// where `seq` is the [`wal_next_seq`] value the append landed
    /// under — handlers that share a background flusher wait for
    /// `wal_synced_seq >= seq` before returning Ack so durability
    /// is preserved while the fsync tail is amortised across many
    /// concurrent Put frames.
    pub fn put_appended(&mut self, k: Key, shard: Shard) -> (bool, u64) {
        use std::sync::atomic::Ordering;
        let h = shard_hash(&shard);
        let epoch = now_epoch();
        if let Some(bucket) = self.shards.get_mut(&k) {
            if let Some((_, e)) = bucket.get_mut(&h) {
                *e = epoch;
                // Dedup: no append. Seq unchanged.
                return (false, self.wal_next_seq.load(Ordering::Acquire));
            }
        }
        let seq = if let Some(wal) = &mut self.wal {
            if let Err(e) = wal.append_put(k.0, k.1, k.2, &h, &shard) {
                eprintln!("Store::put: WAL append failed: {e}");
                return (false, self.wal_next_seq.load(Ordering::Acquire));
            }
            self.wal_next_seq.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            0
        };
        self.shards.entry(k).or_default().insert(h, (shard, epoch));
        (true, seq)
    }

    /// Fsync the WAL and publish the covered seq to
    /// `wal_synced_seq`, then wake any handler waiting for their
    /// seq to become durable. Called from the group-commit flusher
    /// task; a no-op when no WAL is configured.
    pub fn flush_and_publish(&mut self) {
        use std::sync::atomic::Ordering;
        if self.wal.is_none() {
            return;
        }
        let target = self.wal_next_seq.load(Ordering::Acquire);
        let already = self.wal_synced_seq.load(Ordering::Acquire);
        if target == already {
            return;
        }
        let sync_on = self.sync_on_write;
        if let Some(wal) = &mut self.wal {
            if let Err(e) = wal.sync(sync_on) {
                eprintln!("Store::flush_and_publish: WAL sync failed: {e}");
                return;
            }
        }
        self.wal_synced_seq.store(target, Ordering::Release);
        self.wal_notify.notify_waiters();
        self.maybe_rotate_wal();
    }

    /// Rotate the active WAL segment when it crosses the size
    /// threshold. Called after every append; the check is a plain
    /// integer comparison so the amortised cost is O(1). A rotate
    /// failure is logged and the writer keeps the current segment
    /// — better a fat segment than a dropped record.
    fn maybe_rotate_wal(&mut self) {
        let Some(wal) = &mut self.wal else { return };
        if wal.bytes_written() >= wal.rotate_at {
            if let Err(e) = wal.rotate() {
                eprintln!("Store: WAL rotate failed, staying on current segment: {e}");
            }
        }
    }


    pub fn get(&self, k: Key) -> Vec<Shard> {
        self.shards
            .get(&k)
            .map(|m| m.values().map(|(s, _)| s.clone()).collect())
            .unwrap_or_default()
    }

    /// Point lookup by shard hash. Used by the audit: a client checks
    /// that the node actually stores **exactly that** shard.
    pub fn get_by_hash(&self, k: Key, hash: &Hash) -> Option<Shard> {
        self.shards
            .get(&k)
            .and_then(|m| m.get(hash))
            .map(|(s, _)| s.clone())
    }

    pub fn purge(&mut self, object_id: u64) -> usize {
        let mut removed_hashes: Vec<Hash> = Vec::new();
        self.shards.retain(|(o, _, _), bucket| {
            if *o != object_id {
                return true;
            }
            removed_hashes.extend(bucket.keys().copied());
            false
        });
        // Legacy per-shard files (rolling upgrade): still remove
        // them so a downgraded reader can't resurrect them.
        if let Some(dir) = &self.dir {
            for h in &removed_hashes {
                let path = shard_path(dir, h);
                let _ = fs::remove_file(&path);
            }
        }
        if let Some(wal) = &mut self.wal {
            if let Err(e) = wal.append_purge(object_id) {
                eprintln!("Store::purge: WAL append failed: {e}");
            } else if let Err(e) = wal.sync(self.sync_on_write) {
                eprintln!("Store::purge: WAL sync failed: {e}");
            } else {
                self.maybe_rotate_wal();
            }
        }
        removed_hashes.len()
    }

    pub fn total(&self) -> usize {
        self.shards.values().map(|v| v.len()).sum()
    }

    /// list every shard hash this node currently stores,
    /// across every `(object_id, channel, layer)` bucket. Used by the
    /// gateway's GC pass to compute the "held but not referenced
    /// anywhere in the catalog or version archives" delta.
    pub fn list_all_hashes(&self) -> Vec<Hash> {
        let mut out = Vec::with_capacity(self.total());
        for bucket in self.shards.values() {
            out.extend(bucket.keys().copied());
        }
        out
    }

    /// delete every shard whose hash is in `targets`.
    /// Kept for tests and non-GC internal callers; production GC
    /// uses [`Self::purge_by_hashes_up_to`] with the pass's snapshot
    /// epoch so concurrent PUTs are safe.
    pub fn purge_by_hashes(&mut self, targets: &std::collections::HashSet<Hash>) -> usize {
        self.purge_by_hashes_up_to(targets, WriteEpoch::MAX)
    }

    /// epoch-GC: delete every shard whose hash is in `targets`
    /// AND whose stored write-epoch is `<= max_epoch`. Shards with a
    /// higher epoch survive — they were written after the caller's
    /// snapshot, so the caller's "orphan" verdict is stale for them.
    /// Returns the count actually removed.
    pub fn purge_by_hashes_up_to(
        &mut self,
        targets: &std::collections::HashSet<Hash>,
        max_epoch: WriteEpoch,
    ) -> usize {
        let mut removed = 0usize;
        let mut removed_hashes: Vec<Hash> = Vec::new();
        self.shards.retain(|_, bucket| {
            bucket.retain(|h, (_, epoch)| {
                if targets.contains(h) && *epoch <= max_epoch {
                    removed += 1;
                    removed_hashes.push(*h);
                    false
                } else {
                    true
                }
            });
            !bucket.is_empty()
        });
        if let Some(dir) = &self.dir {
            for h in &removed_hashes {
                let path = shard_path(dir, h);
                let _ = fs::remove_file(&path);
            }
        }
        if !removed_hashes.is_empty() {
            if let Some(wal) = &mut self.wal {
                if let Err(e) = wal.append_purge_hashes(&removed_hashes) {
                    eprintln!("Store::purge_by_hashes: WAL append failed: {e}");
                } else if let Err(e) = wal.sync(self.sync_on_write) {
                    eprintln!("Store::purge_by_hashes: WAL sync failed: {e}");
                } else {
                    self.maybe_rotate_wal();
                }
            }
        }
        removed
    }

    /// epoch-GC: current wall-clock. Snapshotted by the gateway
    /// GC pass before it starts walking the catalog / node held
    /// lists, then passed as `max_epoch` to
    /// [`Self::purge_by_hashes_up_to`].
    pub fn current_epoch(&self) -> WriteEpoch {
        now_epoch()
    }

    /// epoch-GC: peek at a shard's stored write-epoch. Used by
    /// tests + diagnostic paths; there is no wire op for this.
    pub fn epoch_of(&self, k: Key, hash: &Hash) -> Option<WriteEpoch> {
        self.shards.get(&k).and_then(|m| m.get(hash)).map(|(_, e)| *e)
    }

    /// Drop everything (used on "node death + replacement").
    pub fn wipe(&mut self) {
        self.shards.clear();
        if let Some(dir) = &self.dir {
            // Remove the directory contents but keep the directory
            // itself. This also drops every legacy .shard file AND
            // every prior WAL segment. The Wipe record we're about
            // to write becomes the whole future history — anything
            // that made it to disk before is gone.
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        let _ = fs::remove_dir_all(&p);
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
        // Reopen a fresh WAL segment so subsequent Puts go
        // somewhere. We rebuild it rather than trying to append a
        // Wipe to the old file — the old file was just deleted.
        if let (Some(dir), true) = (self.dir.clone(), self.wal.is_some()) {
            match crate::wal::WalWriter::open(dir, 0, self.enc_key) {
                Ok(mut wal) => {
                    // Record the Wipe as the first entry so a
                    // fsync-lagging crash doesn't leave the segment
                    // looking blank.
                    let _ = wal.append_wipe();
                    let _ = wal.sync(self.sync_on_write);
                    self.wal = Some(wal);
                }
                Err(e) => {
                    eprintln!("Store::wipe: WAL reopen failed: {e}");
                    self.wal = None;
                }
            }
        }
    }

    /// Override a shard by key + hash (test-only path for corruption testing).
    pub fn inject_corrupt(&mut self, k: Key, shard: Shard) {
        let h = shard_hash(&shard);
        self.shards.entry(k).or_default().insert(h, (shard, now_epoch()));
    }

    /// Storage directory if persistent; otherwise `None`.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

// === Shard file I/O ========================================================

fn shard_path(dir: &Path, h: &Hash) -> PathBuf {
    let s = hex(h);
    let (head, tail) = s.split_at(2);
    dir.join(head).join(format!("{tail}.shard"))
}

fn write_shard_file(
    dir: &Path,
    k: Key,
    h: &Hash,
    shard: &Shard,
    enc_key: Option<&[u8; crate::crypto::KEY_LEN]>,
    fsync: bool,
) -> io::Result<()> {
    let path = shard_path(dir, h);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("shard.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        // Build the 26-byte header once. It's plaintext in either
        // format and — when enc_key is set — feeds the AES-GCM AAD
        // so any tamper with these bytes trips the tag on decrypt.
        let mut header = [0u8; 26];
        let magic = if enc_key.is_some() {
            SHARD_MAGIC_V2
        } else {
            SHARD_MAGIC_V1
        };
        header[..8].copy_from_slice(magic);
        header[8..16].copy_from_slice(&k.0.to_be_bytes());
        header[16] = k.1;
        header[17] = k.2;
        header[18..22].copy_from_slice(&(shard.coeffs.len() as u32).to_be_bytes());
        header[22..26].copy_from_slice(&(shard.payload.len() as u32).to_be_bytes());
        f.write_all(&header)?;

        // v1 = plaintext body; v2 = [nonce | ct+tag] under GCM with
        // the header as AAD.
        match enc_key {
            None => {
                f.write_all(&shard.coeffs)?;
                f.write_all(&shard.payload)?;
            }
            Some(key) => {
                let mut plaintext = Vec::with_capacity(shard.coeffs.len() + shard.payload.len());
                plaintext.extend_from_slice(&shard.coeffs);
                plaintext.extend_from_slice(&shard.payload);
                let sealed = crate::crypto::encrypt(key, &header, &plaintext);
                f.write_all(&sealed)?;
            }
        }
        if fsync {
            f.sync_all()?;
        }
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_shard_file(
    path: &Path,
    enc_key: Option<&[u8; crate::crypto::KEY_LEN]>,
) -> io::Result<(Key, Hash, Shard)> {
    let mut f = fs::File::open(path)?;
    let mut header = [0u8; 26];
    f.read_exact(&mut header)?;
    let magic = &header[..8];
    let object_id = u64::from_be_bytes(header[8..16].try_into().unwrap());
    let channel = header[16];
    let layer = header[17];
    let coeffs_len = u32::from_be_bytes(header[18..22].try_into().unwrap()) as usize;
    let payload_len = u32::from_be_bytes(header[22..26].try_into().unwrap()) as usize;

    let (coeffs, payload) = if magic == SHARD_MAGIC_V1 {
        // Legacy plaintext file — enc_key is irrelevant.
        let mut coeffs = vec![0u8; coeffs_len];
        f.read_exact(&mut coeffs)?;
        let mut payload = vec![0u8; payload_len];
        f.read_exact(&mut payload)?;
        (coeffs, payload)
    } else if magic == SHARD_MAGIC_V2 {
        // Sealed file — need the key or we can't recover the body.
        let key = enc_key.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "shard file is sealed (HOLOFSS2) but no encryption key configured",
            )
        })?;
        let mut sealed = Vec::new();
        f.read_to_end(&mut sealed)?;
        let plaintext = crate::crypto::decrypt(key, &header, &sealed).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("shard decrypt: {e}"))
        })?;
        if plaintext.len() != coeffs_len + payload_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decrypted shard length mismatch: got {}, header wants {}+{}",
                    plaintext.len(),
                    coeffs_len,
                    payload_len,
                ),
            ));
        }
        let (c_slice, p_slice) = plaintext.split_at(coeffs_len);
        (c_slice.to_vec(), p_slice.to_vec())
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a holofs shard file (unknown magic)",
        ));
    };

    let shard = Shard { coeffs, payload };
    let h = shard_hash(&shard);
    Ok(((object_id, channel, layer), h, shard))
}

fn walk_shard_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for top in fs::read_dir(dir)? {
        let top = top?;
        if !top.file_type()?.is_dir() {
            continue;
        }
        for inner in fs::read_dir(top.path())? {
            let inner = inner?;
            let p = inner.path();
            if p.extension().and_then(|s| s.to_str()) == Some("shard") {
                out.push(p);
            }
        }
    }
    Ok(out)
}

pub type SharedStore = Arc<Mutex<Store>>;

/// What `spawn_node` returns to callers: address, store, node identity, handle.
/// The identity is either randomly generated (in-memory mode) or loaded from
/// `storage_dir/identity.key` (persistent mode).
pub struct NodeHandle {
    pub addr: SocketAddr,
    pub store: SharedStore,
    pub identity: NodeIdentity,
    pub task: tokio::task::JoinHandle<()>,
}

/// Start an in-memory node: bind `addr` and serve frames. The identity is
/// randomly generated (it changes across restarts). For the legacy contract
/// `(addr, store, handle)` see the legacy wrapper below.
pub async fn spawn_node(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    let h = spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None).await?;
    Ok((h.addr, h.store, h.task))
}

/// Start a persistent node: data lives in `storage_dir`, identity too
/// (`identity.key`), index is rebuilt from a filesystem scan.
pub async fn spawn_node_persistent(
    addr: SocketAddr,
    storage_dir: impl AsRef<Path>,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    spawn_node_persistent_with_tls(addr, storage_dir, None).await
}

/// Same as [`spawn_node_persistent`] but wraps every accepted connection in
/// TLS using `tls`. `tls = None` falls back to plain TCP — backward
/// compatibility for callers that have not migrated to yet.
pub async fn spawn_node_persistent_with_tls(
    addr: SocketAddr,
    storage_dir: impl AsRef<Path>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    let dir = storage_dir.as_ref().to_path_buf();
    // opt in to at-rest shard encryption via
    // `HOLOFS_AT_REST_ENC=1`. Key material comes from the node's
    // own identity seed — no new secret to manage. Reads accept
    // both plaintext (v1) and sealed (v2) files, so nothing needs
    // to move on a rolling upgrade.
    let identity = NodeIdentity::load_or_create(dir.join("identity.key"))?;
    let at_rest_on = std::env::var("HOLOFS_AT_REST_ENC")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let mut store = if at_rest_on {
        let key = crate::crypto::derive_shard_key(&identity.to_bytes());
        Store::open_with_key(&dir, key)?
    } else {
        Store::open(&dir)?
    };
    // Per-shard fsync default = ON. Operators trade the durability
    // barrier for ~13× encoder throughput by setting
    // HOLOFS_NODE_FSYNC=0 (RLNC replication tolerates the loss).
    let fsync_on = std::env::var("HOLOFS_NODE_FSYNC")
        .ok()
        .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
        .unwrap_or(true);
    store.set_sync_on_write(fsync_on);
    let h = spawn_node_with_identity(addr, store, identity, tls).await?;
    Ok((h.addr, h.store, h.task))
}

/// Start an in-memory node with a specific identity. Returns a struct with
/// the node's pubkey — needed for building a whitelist.
pub async fn spawn_node_full(addr: SocketAddr) -> io::Result<NodeHandle> {
    spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None).await
}

async fn spawn_node_with_identity(
    addr: SocketAddr,
    store: Store,
    identity: NodeIdentity,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<NodeHandle> {
    let store: SharedStore = Arc::new(Mutex::new(store));
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;

    // Group-commit flusher: every 5 ms take the store lock, fsync
    // the WAL if new appends have landed, and wake every waiter.
    // Under a 24-encoder burst this turns 24 × 12 = 288 concurrent
    // per-shard fsyncs into ~200 batched fsyncs/sec (one per
    // 5 ms tick) with each batch amortising N pending appends.
    //
    // 5 ms is a compromise: shorter tightens the tail latency
    // (each handler waits ≤ 5 ms for its fsync) at the cost of
    // more idle wakeups; longer batches more but stalls callers.
    // Tune via `HOLOFS_NODE_FLUSH_INTERVAL_MS` — 0 disables the
    // flusher entirely, which reverts to the old per-Put fsync
    // path via `Store::put`.
    let flush_interval_ms: u64 = std::env::var("HOLOFS_NODE_FLUSH_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    if flush_interval_ms > 0 {
        let store_for_flusher = store.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(
                flush_interval_ms,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Phase 1 (sync, under Store lock): snapshot the seq
                // we'll flush, push the BufWriter's memory chunk to
                // the kernel, hand out a `File` clone for the fsync.
                use std::sync::atomic::Ordering;
                let phase1: Option<(u64, bool, std::fs::File)> = {
                    let mut s = store_for_flusher.lock().await;
                    if s.wal.is_none() {
                        None
                    } else {
                        let target = s.wal_next_seq.load(Ordering::Acquire);
                        let already = s.wal_synced_seq.load(Ordering::Acquire);
                        if target == already {
                            None
                        } else {
                            let sync_on = s.sync_on_write;
                            let file_res = s.wal.as_mut().unwrap().flush_and_take_file();
                            match file_res {
                                Ok(f) => Some((target, sync_on, f)),
                                Err(e) => {
                                    eprintln!(
                                        "Store::flush_and_publish: flush_and_take_file failed: {e}"
                                    );
                                    None
                                }
                            }
                        }
                    }
                };
                let Some((target, sync_on, file)) = phase1 else {
                    continue;
                };
                // Phase 2 (blocking, on the blocking pool): the
                // actual kernel fsync. Under 50-worker load a
                // 5-30 ms fsync used to stall the flusher's
                // tokio worker; other tasks work-stole away, but
                // any `notify_waiters` follower spinning on the
                // same worker was frozen. Splitting the phase
                // gets the worker back for that stall window.
                if sync_on {
                    match tokio::task::spawn_blocking(move || file.sync_all()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            eprintln!("Store::flush_and_publish: WAL fsync failed: {e}");
                            continue;
                        }
                        Err(join) => {
                            eprintln!(
                                "Store::flush_and_publish: fsync task panicked: {join}"
                            );
                            continue;
                        }
                    }
                }
                // Phase 3 (sync, under Store lock): publish the
                // covered seq and wake waiters.
                let mut s = store_for_flusher.lock().await;
                s.wal_synced_seq.store(target, Ordering::Release);
                s.wal_notify.notify_waiters();
                s.maybe_rotate_wal();
            }
        });
    }

    let store_for_task = store.clone();
    let identity_for_task = identity.clone();
    let acceptor = tls.clone().map(tokio_rustls::TlsAcceptor::from);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("accept: {e}");
                    return;
                }
            };
            let store = store_for_task.clone();
            let identity = identity_for_task.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let result = match acceptor {
                    None => handle_connection(stream, store, identity).await,
                    Some(acc) => match acc.accept(stream).await {
                        Ok(tls_stream) => {
                            handle_connection(tls_stream, store, identity).await
                        }
                        Err(e) => Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            format!("TLS handshake: {e}"),
                        )),
                    },
                };
                if let Err(e) = result {
                    // Client connection closed — that's fine; we only log loud failures.
                    if e.kind() != io::ErrorKind::UnexpectedEof
                        && e.kind() != io::ErrorKind::ConnectionReset
                    {
                        eprintln!("node connection error: {e}");
                    }
                }
            });
        }
    });
    Ok(NodeHandle {
        addr: bound,
        store,
        identity,
        task,
    })
}

async fn handle_connection<S>(
    mut stream: S,
    store: SharedStore,
    identity: NodeIdentity,
) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let buf = read_frame(&mut stream).await?;
        let req = Request::decode(&buf)?;
        let resp = handle_request(req, &store, &identity).await;
        write_frame(&mut stream, &resp.encode()).await?;
    }
}

/// Wait until the group-commit flusher has published `my_seq`.
/// Subscribes to `notify` BEFORE the compare so we can't miss the
/// tick between the load and the await — the classic tokio
/// notify race.
async fn wait_for_wal_seq(
    synced_seq: &Arc<std::sync::atomic::AtomicU64>,
    notify: &Arc<tokio::sync::Notify>,
    my_seq: u64,
) {
    use std::sync::atomic::Ordering;
    if my_seq == 0 {
        return; // in-memory store — no WAL to wait on.
    }
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        if synced_seq.load(Ordering::Acquire) >= my_seq {
            return;
        }
        notified.as_mut().await;
    }
}

async fn handle_request(req: Request, store: &SharedStore, identity: &NodeIdentity) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Put {
            object_id,
            channel,
            layer,
            shard,
        } => {
            let t_lock = std::time::Instant::now();
            let mut s = store.lock().await;
            let lock_ns = t_lock.elapsed().as_nanos() as u64;
            let t_append = std::time::Instant::now();
            let (_ok, my_seq) = s.put_appended((object_id, channel, layer), shard);
            let synced_seq = Arc::clone(&s.wal_synced_seq);
            let notify = Arc::clone(&s.wal_notify);
            drop(s);
            let append_ns = t_append.elapsed().as_nanos() as u64;
            let t_wal = std::time::Instant::now();
            wait_for_wal_seq(&synced_seq, &notify, my_seq).await;
            let wal_ns = t_wal.elapsed().as_nanos() as u64;
            use std::sync::atomic::Ordering;
            NODE_PUT_LOCK_WAIT_NS_SUM.fetch_add(lock_ns, Ordering::Relaxed);
            NODE_PUT_APPEND_NS_SUM.fetch_add(append_ns, Ordering::Relaxed);
            NODE_PUT_WAL_WAIT_NS_SUM.fetch_add(wal_ns, Ordering::Relaxed);
            NODE_PUT_COUNT.fetch_add(1, Ordering::Relaxed);
            Response::Ack
        }
        Request::Get {
            object_id,
            channel,
            layer,
        } => {
            let s = store.lock().await;
            Response::Shards(s.get((object_id, channel, layer)))
        }
        Request::Purge { object_id } => {
            let mut s = store.lock().await;
            s.purge(object_id);
            Response::Ack
        }
        Request::Stat => {
            let s = store.lock().await;
            Response::StatResp {
                total_shards: s.total() as u32,
            }
        }
        Request::Audit {
            object_id,
            channel,
            layer,
            shard_hash,
        } => {
            let s = store.lock().await;
            let shard = s.get_by_hash((object_id, channel, layer), &shard_hash);
            Response::AuditResp { shard }
        }
        Request::AuthChallenge { nonce } => {
            let signature = identity.sign_challenge(&nonce);
            Response::AuthChallengeOk { signature }
        }
        Request::ListHashes => {
            let s = store.lock().await;
            Response::Hashes(s.list_all_hashes())
        }
        Request::PurgeByHash { hashes } => {
            let set: std::collections::HashSet<Hash> = hashes.into_iter().collect();
            let mut s = store.lock().await;
            s.purge_by_hashes(&set);
            Response::Ack
        }
        Request::PutBatch {
            object_id,
            channel,
            layer,
            shards,
        } => {
            let t_lock = std::time::Instant::now();
            let mut s = store.lock().await;
            let lock_ns = t_lock.elapsed().as_nanos() as u64;
            let t_append = std::time::Instant::now();
            let mut last_seq: u64 = 0;
            for shard in shards {
                let (_ok, seq) = s.put_appended((object_id, channel, layer), shard);
                if seq > last_seq {
                    last_seq = seq;
                }
            }
            let synced_seq = Arc::clone(&s.wal_synced_seq);
            let notify = Arc::clone(&s.wal_notify);
            drop(s);
            let append_ns = t_append.elapsed().as_nanos() as u64;
            let t_wal = std::time::Instant::now();
            wait_for_wal_seq(&synced_seq, &notify, last_seq).await;
            let wal_ns = t_wal.elapsed().as_nanos() as u64;
            use std::sync::atomic::Ordering;
            NODE_PUT_LOCK_WAIT_NS_SUM.fetch_add(lock_ns, Ordering::Relaxed);
            NODE_PUT_APPEND_NS_SUM.fetch_add(append_ns, Ordering::Relaxed);
            NODE_PUT_WAL_WAIT_NS_SUM.fetch_add(wal_ns, Ordering::Relaxed);
            NODE_PUT_COUNT.fetch_add(1, Ordering::Relaxed);
            Response::Ack
        }
        Request::CurrentEpoch => {
            // Wall-clock: independent of what's in the store.
            // Answering without acquiring the mutex keeps GC pass
            // startup off the critical path of any concurrent PUT.
            Response::Epoch { epoch: now_epoch() }
        }
        Request::PurgeByHashUpTo { hashes, max_epoch } => {
            let set: std::collections::HashSet<Hash> = hashes.into_iter().collect();
            let mut s = store.lock().await;
            s.purge_by_hashes_up_to(&set, max_epoch);
            Response::Ack
        }
        Request::PutTimings => {
            use std::sync::atomic::Ordering;
            Response::PutTimings {
                lock_wait_ns: NODE_PUT_LOCK_WAIT_NS_SUM.load(Ordering::Relaxed),
                put_appended_ns: NODE_PUT_APPEND_NS_SUM.load(Ordering::Relaxed),
                wal_wait_ns: NODE_PUT_WAL_WAIT_NS_SUM.load(Ordering::Relaxed),
                count: NODE_PUT_COUNT.load(Ordering::Relaxed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn make_shard(seed: u8) -> Shard {
        Shard {
            coeffs: vec![seed, seed.wrapping_add(1), seed.wrapping_add(2)],
            payload: vec![0xAA, 0xBB, seed, seed.wrapping_add(7)],
        }
    }

    async fn rpc(stream: &mut TcpStream, req: Request) -> Response {
        let bytes = req.encode();
        let len = bytes.len() as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        let mut lb = [0u8; 4];
        stream.read_exact(&mut lb).await.unwrap();
        let n = u32::from_be_bytes(lb) as usize;
        let mut payload = vec![0u8; n];
        stream.read_exact(&mut payload).await.unwrap();
        Response::decode(&payload).unwrap()
    }

    #[tokio::test]
    async fn ping_pong() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(rpc(&mut s, Request::Ping).await, Response::Pong);
    }

    #[tokio::test]
    async fn put_then_get_returns_shards() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        let sh1 = make_shard(1);
        let sh2 = make_shard(2);
        rpc(
            &mut s,
            Request::Put {
                object_id: 7,
                channel: 0,
                layer: 1,
                shard: sh1.clone(),
            },
        )
        .await;
        rpc(
            &mut s,
            Request::Put {
                object_id: 7,
                channel: 0,
                layer: 1,
                shard: sh2.clone(),
            },
        )
        .await;
        match rpc(
            &mut s,
            Request::Get {
                object_id: 7,
                channel: 0,
                layer: 1,
            },
        )
        .await
        {
            Response::Shards(v) => {
                assert_eq!(v.len(), 2);
                // HashMap order is undefined — compare as a set.
                let set: std::collections::HashSet<_> = v.into_iter().collect();
                assert!(set.contains(&sh1) && set.contains(&sh2));
            }
            other => panic!("expected Shards, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_dedupes_identical_shards() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        let sh = make_shard(42);
        for _ in 0..5 {
            rpc(
                &mut s,
                Request::Put {
                    object_id: 1,
                    channel: 0,
                    layer: 0,
                    shard: sh.clone(),
                },
            )
            .await;
        }
        // All 5 are identical → only 1 must remain.
        assert_eq!(
            rpc(&mut s, Request::Stat).await,
            Response::StatResp { total_shards: 1 }
        );
    }

    #[tokio::test]
    async fn purge_removes_only_target_object() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        for obj in [10u64, 20] {
            rpc(
                &mut s,
                Request::Put {
                    object_id: obj,
                    channel: 0,
                    layer: 0,
                    shard: make_shard(obj as u8),
                },
            )
            .await;
        }
        rpc(&mut s, Request::Purge { object_id: 10 }).await;
        let stat = rpc(&mut s, Request::Stat).await;
        assert_eq!(stat, Response::StatResp { total_shards: 1 });
        let v = rpc(
            &mut s,
            Request::Get {
                object_id: 20,
                channel: 0,
                layer: 0,
            },
        )
        .await;
        match v {
            Response::Shards(s) => assert_eq!(s.len(), 1),
            o => panic!("{o:?}"),
        }
    }

    fn tmpdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "holofs-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn persistent_store_roundtrip_through_restart() {
        let dir = tmpdir("roundtrip");

        // open, write a couple of shards, close.
        let mut s1 = Store::open(&dir).unwrap();
        let sh1 = make_shard(1);
        let sh2 = make_shard(2);
        assert!(s1.put((42, 0, 1), sh1.clone()));
        assert!(s1.put((42, 0, 1), sh2.clone()));
        assert_eq!(s1.total(), 2);
        drop(s1);

        // open again — index is rebuilt from files.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 2);
        let got = s2.get((42, 0, 1));
        let set: std::collections::HashSet<_> = got.into_iter().collect();
        assert!(set.contains(&sh1) && set.contains(&sh2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encrypted_store_roundtrip_and_disk_ciphertext() {
        // Under WAL the sealed record's ciphertext lives inside a
        // segment file (HOLOFSW1). Reading it back reconstructs
        // the shard; inspecting the segment bytes confirms the
        // payload is not stored plaintext.
        let dir = tmpdir("enc-roundtrip");
        let key = crate::crypto::derive_shard_key(&[0xEE; 32]);

        let mut s1 = Store::open_with_key(&dir, key).unwrap();
        let sh = make_shard(0xAA);
        assert!(s1.put((100, 1, 2), sh.clone()));
        drop(s1);

        let segments = crate::wal::walk_wal_segments(&dir).unwrap();
        assert_eq!(segments.len(), 1);
        let raw = std::fs::read(&segments[0].1).unwrap();
        assert_eq!(&raw[..8], crate::wal::SEG_MAGIC);
        assert!(
            !raw.windows(sh.payload.len()).any(|w| w == sh.payload.as_slice()),
            "encrypted WAL segment contains plaintext payload — GCM broken?"
        );

        let s2 = Store::open_with_key(&dir, key).unwrap();
        assert_eq!(s2.total(), 1);
        assert_eq!(s2.get((100, 1, 2)), vec![sh.clone()]);

        // Reopening WITHOUT a key hits a sealed record and refuses
        // to decode it — the read_segment call returns Err, which
        // is wrapped into an io::Error by open_inner.
        assert!(Store::open(&dir).is_err());

        // WRONG key: read_segment surfaces the AEAD failure the
        // same way (decrypt returns Err → Err propagated).
        let wrong_key = crate::crypto::derive_shard_key(&[0xDD; 32]);
        assert!(Store::open_with_key(&dir, wrong_key).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mixed_format_directory_reads_both_v1_and_v2() {
        // Rolling-upgrade path: a legacy per-shard `.shard` file
        // sits next to WAL segments. `open` walks both — legacy
        // for backward compat, WAL for new writes — so a partially
        // migrated directory keeps every shard visible.
        let dir = tmpdir("mixed");

        // Seed a legacy per-shard file by hand — no live code path
        // still writes them, so we call `write_shard_file` directly.
        let sh_legacy = make_shard(1);
        let h_legacy = shard_hash(&sh_legacy);
        write_shard_file(&dir, (1, 0, 0), &h_legacy, &sh_legacy, None, true).unwrap();

        // Second half: normal WAL writes on top.
        {
            let mut s1 = Store::open(&dir).unwrap();
            s1.put((2, 0, 0), make_shard(2));
        }
        // Re-open: both must be visible.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 2);
        assert_eq!(s2.get((1, 0, 0)), vec![make_shard(1)]);
        assert_eq!(s2.get((2, 0, 0)), vec![make_shard(2)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistent_store_dedupes_on_disk() {
        let dir = tmpdir("dedupe");
        let mut s = Store::open(&dir).unwrap();
        let sh = make_shard(7);
        for _ in 0..5 {
            s.put((1, 0, 0), sh.clone());
        }
        assert_eq!(s.total(), 1);
        drop(s);
        // After WAL boot the RAM index must still count as one,
        // proving disk dedup (only the first Put record appended,
        // subsequent re-PUTs took the RAM dedup branch).
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 1);
        // And exactly one WAL segment holds the record.
        let segments = crate::wal::walk_wal_segments(&dir).unwrap();
        assert!(!segments.is_empty(), "WAL segment file must exist");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistent_purge_removes_files() {
        let dir = tmpdir("purge");
        let mut s = Store::open(&dir).unwrap();
        s.put((10, 0, 0), make_shard(1));
        s.put((20, 0, 0), make_shard(2));
        assert_eq!(s.total(), 2);
        s.purge(10);
        assert_eq!(s.total(), 1);
        drop(s);
        // Restart and confirm the Purge record made the state
        // durable — the purged object must not reappear.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 1);
        assert!(s2.get((10, 0, 0)).is_empty());
        assert_eq!(s2.get((20, 0, 0)).len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_up_to_skips_shards_written_after_snapshot() {
        // epoch-GC: a shard whose stored epoch is strictly
        // greater than the caller's `max_epoch` must NOT be purged
        // even when its hash is in the target set. Simulates a
        // concurrent PUT that lands between the GC's held-list
        // snapshot and its purge RPC.
        let mut s = Store::new();
        let sh_old = make_shard(1);
        let h_old = shard_hash(&sh_old);
        s.put((1, 0, 0), sh_old);
        // Force a strictly greater epoch on the "fresh" write so
        // the test is time-independent (`now_epoch()` may not tick
        // between two very-fast calls on some clocks).
        let snapshot = s.epoch_of((1, 0, 0), &h_old).unwrap();
        let sh_new = make_shard(2);
        let h_new = shard_hash(&sh_new);
        s.put((1, 0, 0), sh_new);
        // Overwrite the fresh entry's epoch to a known value > snapshot.
        s.shards
            .get_mut(&(1, 0, 0))
            .unwrap()
            .get_mut(&h_new)
            .unwrap()
            .1 = snapshot + 1000;
        let mut targets = std::collections::HashSet::new();
        targets.insert(h_old);
        targets.insert(h_new);
        // Snapshot only covers the old shard.
        let removed = s.purge_by_hashes_up_to(&targets, snapshot);
        assert_eq!(removed, 1, "only old shard should be purged");
        assert!(s.get_by_hash((1, 0, 0), &h_new).is_some());
        assert!(s.get_by_hash((1, 0, 0), &h_old).is_none());
    }

    #[test]
    fn re_put_bumps_epoch() {
        // Dedup-path re-PUT is a "still live" signal — the entry's
        // epoch must move forward so a GC snapshot taken between the
        // two PUTs can't purge it.
        let mut s = Store::new();
        let sh = make_shard(1);
        let h = shard_hash(&sh);
        assert!(s.put((0, 0, 0), sh.clone()));
        let e1 = s.epoch_of((0, 0, 0), &h).unwrap();
        // Simulate a slow clock — pin e1 down artificially, then
        // re-PUT and confirm the epoch updates.
        s.shards
            .get_mut(&(0, 0, 0))
            .unwrap()
            .get_mut(&h)
            .unwrap()
            .1 = e1.saturating_sub(1_000);
        let pinned = s.epoch_of((0, 0, 0), &h).unwrap();
        assert!(!s.put((0, 0, 0), sh));
        let e2 = s.epoch_of((0, 0, 0), &h).unwrap();
        assert!(e2 > pinned, "re-PUT should bump epoch: {pinned} -> {e2}");
    }

    #[test]
    fn persistent_wipe_clears_files() {
        let dir = tmpdir("wipe");
        let mut s = Store::open(&dir).unwrap();
        for i in 0..4 {
            s.put((1, 0, 0), make_shard(i));
        }
        assert!(s.total() >= 1);
        s.wipe();
        assert_eq!(s.total(), 0);
        drop(s);
        // Wipe replaces the segment file entirely — the reopen
        // sees an empty RAM index. Legacy .shard files should be
        // gone too so a downgraded reader can't resurrect them.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 0);
        assert_eq!(walk_shard_files(&dir).unwrap().len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn persistent_spawn_node_survives_restart() {
        let dir = tmpdir("spawn-restart");

        // start a persistent node, write shards via RPC, shut down.
        let (addr1, _store1, handle1) =
            spawn_node_persistent((Ipv4Addr::LOCALHOST, 0).into(), &dir)
                .await
                .unwrap();
        let mut s = TcpStream::connect(addr1).await.unwrap();
        let sh1 = make_shard(5);
        let sh2 = make_shard(9);
        rpc(
            &mut s,
            Request::Put {
                object_id: 100,
                channel: 1,
                layer: 2,
                shard: sh1.clone(),
            },
        )
        .await;
        rpc(
            &mut s,
            Request::Put {
                object_id: 100,
                channel: 1,
                layer: 2,
                shard: sh2.clone(),
            },
        )
        .await;
        drop(s);
        handle1.abort();
        // Not strictly necessary, but give drop a tick to close the listener.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // same storage_dir — the data must be there.
        let (addr2, _store2, handle2) =
            spawn_node_persistent((Ipv4Addr::LOCALHOST, 0).into(), &dir)
                .await
                .unwrap();
        let mut s = TcpStream::connect(addr2).await.unwrap();
        let resp = rpc(&mut s, Request::Stat).await;
        assert_eq!(resp, Response::StatResp { total_shards: 2 });
        match rpc(
            &mut s,
            Request::Get {
                object_id: 100,
                channel: 1,
                layer: 2,
            },
        )
        .await
        {
            Response::Shards(v) => {
                let set: std::collections::HashSet<_> = v.into_iter().collect();
                assert!(set.contains(&sh1) && set.contains(&sh2));
            }
            other => panic!("expected Shards, got {other:?}"),
        }
        handle2.abort();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stat_counts_total() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        for i in 0..7 {
            rpc(
                &mut s,
                Request::Put {
                    object_id: 1,
                    channel: 0,
                    layer: 0,
                    shard: make_shard(i),
                },
            )
            .await;
        }
        assert_eq!(
            rpc(&mut s, Request::Stat).await,
            Response::StatResp { total_shards: 7 }
        );
    }
}
