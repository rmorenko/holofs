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

/// Node-service knobs read from `HOLOFS_*` env vars at `spawn_node`
/// start-up. v2 P4.4 collects the three settings into one typed
/// snapshot so the env vocabulary lives in a single place per-crate,
/// mirroring what `holofs_web::runtime_config::RuntimeConfig` does for
/// the web layer. All fields are start-up only (`spawn_node` reads
/// them once); a `set_var` after boot has no effect on an already-
/// running node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Opt in to at-rest shard encryption via `HOLOFS_AT_REST_ENC=1`.
    /// Key material derives from the node's own identity seed.
    pub at_rest_encryption: bool,
    /// Per-shard fsync. Default `true`; operators trade the
    /// durability barrier for ~13× encoder throughput by setting
    /// `HOLOFS_NODE_FSYNC=0`.
    pub fsync_on_write: bool,
    /// Group-commit flusher tick interval, silently clamped to ≥ 1 ms
    /// (see B3 in `holofs-review.md`). Configurable via
    /// `HOLOFS_NODE_FLUSH_INTERVAL_MS`; default 5 ms.
    pub flush_interval_ms: u64,
    /// Background WAL compactor tick period, in seconds. On each
    /// tick the compactor decides whether to run based on
    /// `wal_compact_ratio` + `wal_compact_min_bytes`. Set to `0` to
    /// disable the compactor entirely (segments accumulate as in the
    /// pre-2.x era). Env: `HOLOFS_WAL_COMPACT_INTERVAL_SECS`
    /// (default 300 = 5 min).
    pub wal_compact_interval_secs: u64,
    /// Compaction ratio threshold. On each compactor tick, if
    /// `wal_disk_bytes / max(live_bytes_estimate, 1) > ratio` AND
    /// disk usage is above `wal_compact_min_bytes`, run a compaction
    /// early (before the interval elapses again). Ratio > 2 means
    /// the log has doubled the live payload — reclaiming makes sense.
    /// Env: `HOLOFS_WAL_COMPACT_RATIO` (default 2.0).
    pub wal_compact_ratio: f64,
    /// Floor on disk usage before either the interval or the ratio
    /// trigger fires. Prevents busy-looping compaction on tiny logs
    /// where compacted output can be as large as the input. Env:
    /// `HOLOFS_WAL_COMPACT_MIN_BYTES` (default 4 MiB).
    pub wal_compact_min_bytes: u64,
    /// P1.7 — where the KEK (Key Encryption Key that wraps every
    /// DEK in the on-disk keyring) comes from. Only consulted when
    /// `at_rest_encryption` is `true`. Env:
    /// `HOLOFS_AT_REST_KEK_SOURCE` (default `identity` = pre-P1.7
    /// backwards-compat, HKDF from the node's identity seed).
    pub kek_source: crate::crypto::KekSource,
    /// P1.7 — override path for `keyring.json`. Env:
    /// `HOLOFS_KEYRING_PATH`. `None` ⇒ `<storage>/keyring.json`.
    pub keyring_path: Option<std::path::PathBuf>,
}

impl NodeConfig {
    /// Read the current environment. Called from `spawn_node`; kept
    /// public so tests / benchmarks that construct nodes without
    /// going through `spawn_node` can share the same env vocabulary.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            at_rest_encryption: std::env::var("HOLOFS_AT_REST_ENC")
                .ok()
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
            fsync_on_write: std::env::var("HOLOFS_NODE_FSYNC")
                .ok()
                .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
                .unwrap_or(true),
            flush_interval_ms: std::env::var("HOLOFS_NODE_FLUSH_INTERVAL_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5)
                .max(1),
            wal_compact_interval_secs: std::env::var("HOLOFS_WAL_COMPACT_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
            wal_compact_ratio: std::env::var("HOLOFS_WAL_COMPACT_RATIO")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2.0),
            wal_compact_min_bytes: std::env::var("HOLOFS_WAL_COMPACT_MIN_BYTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4 * 1024 * 1024),
            kek_source: match std::env::var("HOLOFS_AT_REST_KEK_SOURCE")
                .unwrap_or_else(|_| "identity".to_string())
                .as_str()
            {
                "file" => crate::crypto::KekSource::File(
                    std::env::var("HOLOFS_AT_REST_KEK_PATH")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| {
                            eprintln!(
                                "HOLOFS_AT_REST_KEK_SOURCE=file but HOLOFS_AT_REST_KEK_PATH \
                                 unset — falling back to `identity`"
                            );
                            std::path::PathBuf::new()
                        }),
                ),
                "env" => crate::crypto::KekSource::EnvHex(
                    std::env::var("HOLOFS_AT_REST_KEK_HEX").unwrap_or_else(|_| {
                        eprintln!(
                            "HOLOFS_AT_REST_KEK_SOURCE=env but HOLOFS_AT_REST_KEK_HEX \
                             unset — falling back to `identity`"
                        );
                        String::new()
                    }),
                ),
                _ => crate::crypto::KekSource::IdentitySeed,
            },
            keyring_path: std::env::var("HOLOFS_KEYRING_PATH")
                .ok()
                .map(std::path::PathBuf::from),
        }
    }
}

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
    holofs_core::time::now_unix_ms()
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
/// Return value from [`Store::compact`] describing the freshly-written
/// compacted segment and the shape of the live state it captured.
/// Handy for tracing / metrics / soak reports.
#[derive(Debug, Clone)]
pub struct CompactionReport {
    /// Compaction epoch — equals the seq of the segment that was
    /// closed by the rotate that started this compaction. Every
    /// regular `wal-N.seg` with `N ≤ epoch` is now subsumed by
    /// `wal-c<epoch>.seg` and has been garbage-collected.
    pub epoch: u64,
    /// Number of Put records written into the compacted segment
    /// (= live shard count at snapshot time).
    pub records: usize,
    /// Sum of `shard.coeffs.len() + shard.payload.len()` across the
    /// captured live records.
    pub live_bytes: u64,
    /// Final on-disk path of the compacted segment (post atomic
    /// rename). Absolute path under the store's `dir`.
    pub compact_path: PathBuf,
}

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
    /// P1.7 envelope-encryption keyring. `None` = plaintext shard
    /// files on disk (default; no `HOLOFS_AT_REST_ENC=1`). `Some(_)`
    /// = writes seal under the ring's current DEK; reads try every
    /// DEK in the ring so a post-rotation node still decrypts
    /// pre-rotation shards. The ring's raw DEKs are loaded once at
    /// boot from `<storage>/keyring.json` (unwrapped under the
    /// operator's KEK — see [`crate::crypto::KekSource`]) and held
    /// in RAM for the process lifetime.
    keyring: Option<Arc<crate::crypto::Keyring>>,
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
            keyring: None,
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

    /// Persistent store with a single-DEK in-memory keyring — the
    /// pre-P1.7 API preserved for tests + code paths that already
    /// have a raw key. New writes land as `HOLOFSS2` sealed files;
    /// reads accept both `HOLOFSS1` (legacy plaintext) and
    /// `HOLOFSS2` (sealed) transparently.
    ///
    /// Production callers should use [`Self::open_with_keyring`] so
    /// rotation, KEK sources, and on-disk key persistence work.
    pub fn open_with_key(
        dir: impl AsRef<Path>,
        key: [u8; crate::crypto::KEY_LEN],
    ) -> io::Result<Self> {
        let ring = Arc::new(crate::crypto::Keyring::in_memory_single(key));
        Self::open_inner(dir, Some(ring))
    }

    /// Full-fat persistent store: hands over a fully-materialised
    /// [`Keyring`](crate::crypto::Keyring) (already unwrapped under
    /// the operator's KEK). Bootstrap constructs the ring via
    /// [`crate::crypto::Keyring::load_or_bootstrap`] and passes the
    /// Arc'd handle here — the same handle stays in the Store for
    /// every subsequent encrypt/decrypt.
    pub fn open_with_keyring(
        dir: impl AsRef<Path>,
        keyring: Arc<crate::crypto::Keyring>,
    ) -> io::Result<Self> {
        Self::open_inner(dir, Some(keyring))
    }

    fn open_inner(
        dir: impl AsRef<Path>,
        keyring: Option<Arc<crate::crypto::Keyring>>,
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
            match read_shard_file(&entry, keyring.as_deref()) {
                Ok((k, h, shard)) => {
                    shards.entry(k).or_default().insert(h, (shard, epoch));
                }
                Err(e) => {
                    eprintln!("Store::open: skipping broken file {entry:?}: {e}");
                }
            }
        }
        // 2a. Compacted segment (if any). Contains the full live
        //     state at the moment its epoch was compacted; regular
        //     segments with seq <= epoch are subsumed and MUST be
        //     skipped so we don't double-apply their Puts. If more
        //     than one wal-c<N>.seg is present (crash mid-gc) the
        //     highest wins — older ones are garbage from a previous
        //     round that never got cleaned up.
        let compact_epoch = crate::wal::highest_compact_epoch(&dir)?.unwrap_or(0);
        let mut highest_seq: u64 = compact_epoch;
        if compact_epoch > 0 {
            let compact_path = crate::wal::walk_compact_segments(&dir)?
                .into_iter()
                .find(|(e, _)| *e == compact_epoch)
                .map(|(_, p)| p)
                .expect("highest_compact_epoch matched exactly one entry");
            let epoch_from_mtime = fs::metadata(&compact_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let records = crate::wal::read_segment(&compact_path, keyring.as_deref())?;
            for rec in records {
                apply_wal_record_to_shards(&mut shards, rec, epoch_from_mtime);
            }
        }
        // 2b. Regular WAL segments, in seq order — but only those
        //     that came after the last compaction. Records apply on
        //     top of whatever the legacy scan and compacted segment
        //     already produced — Put overwrites, Purge/PurgeHashes/
        //     Wipe drop.
        let segments = crate::wal::walk_wal_segments(&dir)?;
        for (seq, path) in &segments {
            if *seq <= compact_epoch {
                // Subsumed by the compacted segment. Left on disk by
                // a crashed gc pass; the next `Store::compact` will
                // sweep them via `wal::gc_compacted`.
                continue;
            }
            highest_seq = (*seq).max(highest_seq);
            let epoch_from_mtime = fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let records = crate::wal::read_segment(path, keyring.as_deref())?;
            for rec in records {
                apply_wal_record_to_shards(&mut shards, rec, epoch_from_mtime);
            }
        }
        // 3. Open the next segment for writes. Even a brand-new
        //    directory gets seq=1 so writes never share a file with
        //    a replayed segment.
        let wal = crate::wal::WalWriter::open(dir.clone(), highest_seq, keyring.clone())?;
        Ok(Store {
            shards,
            dir: Some(dir),
            wal: Some(wal),
            wal_next_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_synced_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_notify: Arc::new(tokio::sync::Notify::new()),
            keyring,
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

    /// Total bytes across every WAL file on disk — regular segments
    /// plus compacted segments plus any stray `.tmp` orphans. Used by
    /// the background compactor to decide when to run based on the
    /// `total_wal_bytes / live_bytes` ratio. Cheap: one `stat` per
    /// file, no reads.
    pub fn wal_disk_bytes(&self) -> io::Result<u64> {
        let Some(dir) = &self.dir else { return Ok(0) };
        let mut total: u64 = 0;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_wal_file = name.starts_with("wal-")
                && (name.ends_with(".seg") || name.ends_with(".seg.tmp"));
            if !is_wal_file {
                continue;
            }
            total = total.saturating_add(entry.metadata()?.len());
        }
        Ok(total)
    }

    /// Rough live-state size in bytes — sum of every RAM shard's
    /// `coeffs + payload`. The compactor divides `wal_disk_bytes` by
    /// this to derive the write-amplification ratio; a ratio > 2×
    /// means the WAL carries at least as much stale-record overhead
    /// as live data and compaction pays off.
    pub fn live_bytes_estimate(&self) -> u64 {
        self.shards
            .values()
            .flat_map(|bucket| bucket.values())
            .map(|(shard, _)| (shard.coeffs.len() + shard.payload.len()) as u64)
            .sum()
    }

    /// Snapshot the RAM store into a fresh compacted segment, then
    /// garbage-collect every regular / older-compact segment it
    /// subsumes. Called by the background compactor on interval /
    /// threshold trigger.
    ///
    /// Sequence of operations:
    /// 1. `wal.sync(true)` — make sure the active segment is durable
    ///    before we rotate it away.
    /// 2. `wal.rotate()` — close the currently-active segment with
    ///    its V2 footer. Its seq becomes the compaction `epoch`;
    ///    every regular `wal-N.seg` with `N ≤ epoch` will be
    ///    subsumed.
    /// 3. `wal::write_compacted_segment` — write a `wal-c<epoch>.seg`
    ///    carrying the full live state (one Put per (key, hash)
    ///    entry in the shard map). Atomic rename from `.tmp`
    ///    guarantees a crash mid-write leaves an orphan `.tmp` (swept
    ///    on next boot / next compaction) rather than a half-written
    ///    compacted file.
    /// 4. `wal::gc_compacted` — delete every `wal-N.seg` with
    ///    `N ≤ epoch` plus any `wal-c<M>.seg` with `M < epoch`.
    ///
    /// Crash safety: after step 3 succeeds, boot replay picks up the
    /// new compacted segment as the highest one and skips every
    /// subsumed regular segment (whether or not step 4 finished).
    ///
    /// Returns `Ok(None)` if the store isn't backed by disk (nothing
    /// to compact) or if there's nothing worth compacting (only the
    /// bootstrap segment exists and it's empty).
    pub fn compact(&mut self) -> io::Result<Option<CompactionReport>> {
        let Some(wal) = self.wal.as_mut() else {
            return Ok(None);
        };
        let dir = self.dir.clone().expect("wal is Some implies dir is Some");

        // Nothing worth compacting yet: only the bootstrap segment
        // exists and it's essentially empty (magic header only).
        // Rotating just to write an empty compact segment is churn.
        let existing_segs = crate::wal::walk_wal_segments(&dir)?;
        let existing_compacts = crate::wal::walk_compact_segments(&dir)?;
        let has_prior_closed = existing_segs.len() > 1
            || existing_segs.iter().any(|(seq, _)| *seq < wal.active_seq());
        let active_has_data = wal.bytes_written() > 8; // > magic
        if !has_prior_closed && !active_has_data && existing_compacts.is_empty() {
            return Ok(None);
        }

        // 1 + 2. Flush + rotate so the compaction epoch is the seq
        // of a fully-closed, footer-carrying segment.
        wal.sync(true)?;
        let epoch = wal.active_seq();
        wal.rotate()?;

        // 3. Snapshot live state into a shape `write_compacted_segment`
        // consumes. We clone shards here because the writer path
        // needs owned `Shard`s (they get re-encoded); this is O(live
        // bytes) and runs under the store lock — acceptable because
        // compaction is background-tick frequency, not per-request.
        let live_records: Vec<(u64, u8, u8, Hash, Shard)> = self
            .shards
            .iter()
            .flat_map(|((o, c, l), bucket)| {
                let o = *o;
                let c = *c;
                let l = *l;
                bucket.iter().map(move |(h, (s, _))| (o, c, l, *h, s.clone()))
            })
            .collect();
        let record_count = live_records.len();
        let live_bytes: u64 = live_records
            .iter()
            .map(|(_, _, _, _, s)| (s.coeffs.len() + s.payload.len()) as u64)
            .sum();
        let compact_path = crate::wal::write_compacted_segment(
            &dir,
            epoch,
            live_records,
            self.keyring.clone(),
        )?;

        // 4. Sweep subsumed regular + older compacted segments +
        // any stray .tmp orphans. Best-effort — failures here just
        // leave garbage that the next compaction round retries.
        crate::wal::gc_compacted(&dir, epoch)?;

        Ok(Some(CompactionReport {
            epoch,
            records: record_count,
            live_bytes,
            compact_path,
        }))
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

    /// Delete every shard of `object_id`. WAL-first: writes the delete
    /// record to the WAL and fsyncs *before* mutating the in-memory
    /// index. If the WAL write fails we return `Err` without touching
    /// RAM state — the handler up in `node_service` surfaces this as
    /// `Response::Error`. Prior to this the write order was inverted
    /// (RAM → WAL): a WAL failure logged and dropped, but the client
    /// had already been told `Ack`; on restart the WAL replay would
    /// "resurrect" the object that the client believed deleted. See
    /// review v2 §Purge/WAL-Ack.
    pub fn purge(&mut self, object_id: u64) -> io::Result<usize> {
        // Persist the intent first. On a WAL-configured store, this is
        // the boundary at which the delete becomes durable.
        if let Some(wal) = &mut self.wal {
            wal.append_purge(object_id)?;
            wal.sync(self.sync_on_write)?;
            self.maybe_rotate_wal();
        }
        // Now safe to mutate.
        let mut removed_hashes: Vec<Hash> = Vec::new();
        self.shards.retain(|(o, _, _), bucket| {
            if *o != object_id {
                return true;
            }
            removed_hashes.extend(bucket.keys().copied());
            false
        });
        // Legacy per-shard files (rolling upgrade): still remove
        // them so a downgraded reader can't resurrect them. Failures
        // here are best-effort — next GC pass cleans up.
        if let Some(dir) = &self.dir {
            for h in &removed_hashes {
                let path = shard_path(dir, h);
                let _ = fs::remove_file(&path);
            }
        }
        Ok(removed_hashes.len())
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
    pub fn purge_by_hashes(
        &mut self,
        targets: &std::collections::HashSet<Hash>,
    ) -> io::Result<usize> {
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
    ) -> io::Result<usize> {
        // Compute the removal set FIRST without mutating so we can
        // persist the exact list to the WAL and only commit it to
        // RAM after fsync. Same durability boundary as `purge` —
        // see the docstring there. Prior code mutated RAM, then
        // wrote WAL, then logged-and-dropped any WAL error, letting
        // the deletion get rolled back on restart while the caller
        // had already been Ack'd.
        let mut removed_hashes: Vec<Hash> = Vec::new();
        for (_, bucket) in self.shards.iter() {
            for (h, (_, epoch)) in bucket.iter() {
                if targets.contains(h) && *epoch <= max_epoch {
                    removed_hashes.push(*h);
                }
            }
        }
        if removed_hashes.is_empty() {
            return Ok(0);
        }
        if let Some(wal) = &mut self.wal {
            wal.append_purge_hashes(&removed_hashes)?;
            wal.sync(self.sync_on_write)?;
            self.maybe_rotate_wal();
        }
        // Now safe to mutate.
        let doomed: std::collections::HashSet<Hash> =
            removed_hashes.iter().copied().collect();
        let mut removed = 0usize;
        self.shards.retain(|_, bucket| {
            bucket.retain(|h, _| {
                if doomed.contains(h) {
                    removed += 1;
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
        Ok(removed)
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
            match crate::wal::WalWriter::open(dir, 0, self.keyring.clone()) {
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
    keyring: Option<&crate::crypto::Keyring>,
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
        // format and — when a keyring is set — feeds the AES-GCM
        // AAD so any tamper with these bytes trips the tag on
        // decrypt.
        let mut header = [0u8; 26];
        let magic = if keyring.is_some() {
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
        // the header as AAD, using the keyring's current DEK.
        match keyring {
            None => {
                f.write_all(&shard.coeffs)?;
                f.write_all(&shard.payload)?;
            }
            Some(ring) => {
                let mut plaintext = Vec::with_capacity(shard.coeffs.len() + shard.payload.len());
                plaintext.extend_from_slice(&shard.coeffs);
                plaintext.extend_from_slice(&shard.payload);
                let sealed = ring.encrypt_current(&header, &plaintext);
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
    keyring: Option<&crate::crypto::Keyring>,
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
        // Sealed file — need a keyring or we can't recover the body.
        let ring = keyring.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "shard file is sealed (HOLOFSS2) but no keyring configured",
            )
        })?;
        let mut sealed = Vec::new();
        f.read_to_end(&mut sealed)?;
        let plaintext = ring.decrypt_any(&header, &sealed).map_err(|e| {
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

/// Apply one replayed WAL record to the in-memory shard map. Shared
/// between the compacted-segment replay pass and the regular-segment
/// pass so both paths use identical apply semantics — Put overwrites,
/// Purge / PurgeHashes / Wipe drop.
fn apply_wal_record_to_shards(
    shards: &mut HashMap<Key, HashMap<Hash, (Shard, WriteEpoch)>>,
    rec: crate::wal::RecordKind,
    epoch: WriteEpoch,
) {
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
                .insert(hash, (shard, epoch));
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
    let h =
        spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None, None).await?;
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
    spawn_node_persistent_with_tls_and_whitelist(addr, storage_dir, tls, None).await
}

/// Full-fat persistent-node constructor: TLS/mTLS **plus** ingress
/// client whitelist (P0.3b). When `client_whitelist` is `Some`, every
/// incoming connection MUST complete a bilateral
/// [`Request::Handshake`] → [`Request::HandshakeComplete`] exchange
/// before the node accepts any Put/Get/Purge frame — the client
/// pubkey is verified against `client_whitelist`. `None` preserves the
/// pre-P0.3b permissive contract (any TCP peer accepted).
pub async fn spawn_node_persistent_with_tls_and_whitelist(
    addr: SocketAddr,
    storage_dir: impl AsRef<Path>,
    tls: Option<Arc<rustls::ServerConfig>>,
    client_whitelist: Option<Arc<crate::whitelist::Whitelist>>,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    let dir = storage_dir.as_ref().to_path_buf();
    // opt in to at-rest shard encryption via
    // `HOLOFS_AT_REST_ENC=1`. Post-P1.7 the DEK lives in the
    // envelope-encrypted keyring at `<storage>/keyring.json`,
    // unwrapped under the KEK returned by
    // `NodeConfig::kek_source`. When the keyring is missing we
    // bootstrap a single-DEK ring seeded from HKDF(identity) — that
    // gives byte-for-byte backward compat with pre-P1.7 sealed
    // shards. Rotation appends new DEKs; old shards keep decrypting
    // under the retained old DEK.
    let identity = NodeIdentity::load_or_create(dir.join("identity.key"))?;
    let cfg = NodeConfig::from_env();
    let mut store = if cfg.at_rest_encryption {
        let kek = cfg
            .kek_source
            .load(&identity.to_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("KEK: {e}")))?;
        let bootstrap_dek = crate::crypto::derive_shard_key(&identity.to_bytes());
        let keyring_path = cfg
            .keyring_path
            .clone()
            .unwrap_or_else(|| dir.join("keyring.json"));
        let keyring = std::sync::Arc::new(crate::crypto::Keyring::load_or_bootstrap(
            &keyring_path,
            &kek,
            bootstrap_dek,
        )?);
        let (cur, n, age_ms) = keyring.summary();
        eprintln!(
            "holofs-node at-rest: keyring {} ({} DEK(s), current id={}, oldest age {}ms) kek_source={}",
            keyring_path.display(),
            n,
            cur,
            age_ms,
            cfg.kek_source.label(),
        );
        Store::open_with_keyring(&dir, keyring)?
    } else {
        Store::open(&dir)?
    };
    store.set_sync_on_write(cfg.fsync_on_write);
    let h = spawn_node_with_identity(addr, store, identity, tls, client_whitelist).await?;
    Ok((h.addr, h.store, h.task))
}

/// Start an in-memory node with a specific identity. Returns a struct with
/// the node's pubkey — needed for building a whitelist.
pub async fn spawn_node_full(addr: SocketAddr) -> io::Result<NodeHandle> {
    spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None, None).await
}

async fn spawn_node_with_identity(
    addr: SocketAddr,
    store: Store,
    identity: NodeIdentity,
    tls: Option<Arc<rustls::ServerConfig>>,
    client_whitelist: Option<Arc<crate::whitelist::Whitelist>>,
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
    // Tune via `HOLOFS_NODE_FLUSH_INTERVAL_MS` — minimum 1 ms.
    //
    // B3: the previous docstring advertised `0` as "disable the
    // flusher; revert to per-Put fsync via `Store::put`", but the
    // handler always went through `put_appended` +
    // `wait_for_wal_seq` — with no flusher publishing
    // `wal_synced_seq`, every persistent-store `Put` hung forever.
    // Silently clamping to 1 ms is the least-surprising behaviour:
    // it matches the doc's stated goal (aggressive per-tick
    // batching) without breaking the handler contract.
    let flush_interval_ms: u64 = NodeConfig::from_env().flush_interval_ms;
    {
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

    // Background WAL compactor. Ticks at a fixed cadence and, on
    // each tick, decides whether to run based on
    //   - interval: `now - last_compact_at >= wal_compact_interval_secs`
    //     (the safety-net guarantee: on a quiet node, compaction runs
    //     at least once per interval even if the ratio never fires);
    //   - ratio: `wal_disk_bytes / max(live_bytes, 1) > wal_compact_ratio`
    //     (for write-heavy nodes accumulating stale-record overhead
    //     between intervals).
    // Both gated by `wal_compact_min_bytes` so tiny logs don't churn.
    //
    // `wal_compact_interval_secs = 0` disables the loop entirely
    // (segments accumulate as before). In-memory stores (no `wal`)
    // are also a no-op — `Store::compact` returns `Ok(None)`.
    //
    // Tick cadence is `min(30s, interval/10)` so a short custom
    // interval still enforces itself on time.
    let cfg = NodeConfig::from_env();
    if cfg.wal_compact_interval_secs > 0 {
        let store_for_compactor = store.clone();
        let interval_secs = cfg.wal_compact_interval_secs;
        let ratio = cfg.wal_compact_ratio;
        let min_bytes = cfg.wal_compact_min_bytes;
        let tick_secs = interval_secs.min(30).max(1);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the first (immediate) tick — no writes have
            // landed yet on a freshly-booted node, and any compacted
            // segment from the previous boot is already replayed.
            ticker.tick().await;
            let mut last_compact_at = std::time::Instant::now();
            loop {
                ticker.tick().await;
                let (wal_bytes, live_bytes) = {
                    let s = store_for_compactor.lock().await;
                    if s.wal.is_none() {
                        return; // in-memory store: nothing to compact, ever
                    }
                    let wb = s.wal_disk_bytes().unwrap_or(0);
                    let lb = s.live_bytes_estimate();
                    (wb, lb)
                };
                if wal_bytes < min_bytes {
                    continue;
                }
                let elapsed = last_compact_at.elapsed().as_secs();
                let interval_hit = elapsed >= interval_secs;
                let ratio_hit = (wal_bytes as f64) / (live_bytes.max(1) as f64) > ratio;
                if !interval_hit && !ratio_hit {
                    continue;
                }
                let report = {
                    let mut s = store_for_compactor.lock().await;
                    s.compact()
                };
                match report {
                    Ok(Some(r)) => {
                        last_compact_at = std::time::Instant::now();
                        eprintln!(
                            "wal compaction complete: epoch={} records={} live_bytes={} \
                             wal_bytes_before={} trigger={}",
                            r.epoch,
                            r.records,
                            r.live_bytes,
                            wal_bytes,
                            if interval_hit { "interval" } else { "ratio" },
                        );
                    }
                    Ok(None) => {
                        // Nothing to compact yet (bootstrap segment
                        // is empty). Don't reset last_compact_at
                        // so the next tick tries again immediately.
                    }
                    Err(e) => {
                        eprintln!("wal compaction failed: {e}");
                    }
                }
            }
        });
    }

    let store_for_task = store.clone();
    let identity_for_task = identity.clone();
    let acceptor = tls.clone().map(tokio_rustls::TlsAcceptor::from);
    let whitelist_for_task = client_whitelist.clone();
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
            let whitelist = whitelist_for_task.clone();
            tokio::spawn(async move {
                let result = match acceptor {
                    None => handle_connection(stream, store, identity, whitelist).await,
                    Some(acc) => match acc.accept(stream).await {
                        Ok(tls_stream) => {
                            handle_connection(tls_stream, store, identity, whitelist).await
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
    client_whitelist: Option<Arc<crate::whitelist::Whitelist>>,
) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // P0.3b bilateral-auth state. Set by a successful
    // `Request::Handshake` → `Request::HandshakeComplete` round
    // trip; the node then knows the peer identity is a legitimate
    // holder of `client_pubkey`.
    //
    // Strict mode (client_whitelist = Some) rejects every non-
    // handshake frame until this flips true; permissive mode
    // (client_whitelist = None, current contract) treats the flag
    // as advisory — clients may still speak the handshake but the
    // node doesn't require or verify it.
    let mut authenticated = false;
    let strict = client_whitelist.is_some();

    loop {
        let buf = read_frame(&mut stream).await?;
        let req = Request::decode(&buf)?;

        // Bilateral handshake mini-state-machine. Handshake requests
        // are handled inline here (not by `handle_request`) because
        // they own the next round-trip on the wire.
        match req {
            Request::Handshake {
                client_pubkey,
                client_nonce,
            } => {
                match process_handshake(
                    &mut stream,
                    &identity,
                    &client_pubkey,
                    &client_nonce,
                    client_whitelist.as_deref(),
                )
                .await
                {
                    Ok(()) => {
                        authenticated = true;
                        continue;
                    }
                    Err(reason) => {
                        let _ = write_frame(
                            &mut stream,
                            &Response::Error(format!("handshake: {reason}")).encode(),
                        )
                        .await;
                        return Ok(());
                    }
                }
            }
            Request::HandshakeComplete { .. } => {
                // Second-frame arriving alone: protocol violation —
                // must be preceded by `Handshake`.
                let _ = write_frame(
                    &mut stream,
                    &Response::Error("unexpected HandshakeComplete frame".into()).encode(),
                )
                .await;
                return Ok(());
            }
            _ => {}
        }

        // Strict mode: every non-handshake frame requires a
        // completed handshake first. Rejection closes the
        // connection — retry means a fresh TCP + handshake.
        if strict && !authenticated {
            let _ = write_frame(
                &mut stream,
                &Response::Error(
                    "handshake required: node runs with client whitelist enforcement".into(),
                )
                .encode(),
            )
            .await;
            return Ok(());
        }

        let resp = handle_request(req, &store, &identity).await;
        write_frame(&mut stream, &resp.encode()).await?;
    }
}

/// Client-side counterpart to [`process_handshake`]. Runs the
/// bilateral handshake as the initiating party:
///
/// 1. Emit [`Request::Handshake`] carrying our own pubkey + a fresh
///    nonce.
/// 2. Read [`Response::HandshakeChallenge`]; verify the node's
///    signature over our nonce against `expected_node_pubkey`.
/// 3. Sign the `server_nonce` we just received and emit
///    [`Request::HandshakeComplete`].
/// 4. Read final [`Response::Ack`] (any other response = rejection).
///
/// Returns `Ok(())` on a fully-verified round trip, `Err(reason)`
/// otherwise. Callers should treat any error as "connection is
/// unauthenticated" and close the socket.
///
/// Independent of TLS: this runs on top of whatever `AsyncRead +
/// AsyncWrite` stream you hand it. Compose with `TlsConnector` for
/// TLS-inside-authn.
pub async fn perform_bilateral_handshake_as_client<S>(
    stream: &mut S,
    client_identity: &NodeIdentity,
    expected_node_pubkey: &crate::identity::PubKey,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::identity::{fresh_nonce, verify_challenge};

    // Step 1: emit Handshake.
    let client_nonce = fresh_nonce();
    let client_pubkey = client_identity.pubkey();
    write_frame(
        stream,
        &Request::Handshake {
            client_pubkey,
            client_nonce,
        }
        .encode(),
    )
    .await
    .map_err(|e| format!("write handshake: {e}"))?;

    // Step 2: read HandshakeChallenge, verify node signature.
    let buf = read_frame(stream)
        .await
        .map_err(|e| format!("read challenge: {e}"))?;
    let resp = Response::decode(&buf).map_err(|e| format!("decode challenge: {e}"))?;
    let (node_signature, server_nonce) = match resp {
        Response::HandshakeChallenge {
            node_signature,
            server_nonce,
        } => (node_signature, server_nonce),
        Response::Error(msg) => {
            return Err(format!("node rejected handshake: {msg}"));
        }
        other => {
            return Err(format!(
                "expected HandshakeChallenge, got {:?}",
                std::mem::discriminant(&other)
            ));
        }
    };
    if !verify_challenge(expected_node_pubkey, &client_nonce, &node_signature) {
        return Err("node signature over client nonce failed verification".into());
    }

    // Step 3: sign server_nonce, emit HandshakeComplete.
    let client_signature = client_identity.sign_challenge(&server_nonce);
    write_frame(
        stream,
        &Request::HandshakeComplete { client_signature }.encode(),
    )
    .await
    .map_err(|e| format!("write complete: {e}"))?;

    // Step 4: read final Ack.
    let buf = read_frame(stream)
        .await
        .map_err(|e| format!("read final ack: {e}"))?;
    let resp = Response::decode(&buf).map_err(|e| format!("decode final ack: {e}"))?;
    match resp {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(format!("node rejected on complete: {msg}")),
        other => Err(format!(
            "expected Ack after HandshakeComplete, got {:?}",
            std::mem::discriminant(&other)
        )),
    }
}

/// Bilateral handshake helper (P0.3b). Called from
/// [`handle_connection`] as soon as the peer sends
/// [`Request::Handshake`]. Runs the remaining 3 wire steps:
///
/// 1. Verify `client_pubkey` against the ingress whitelist (skipped
///    when the whitelist is `None` — permissive mode).
/// 2. Sign `client_nonce` with the node's identity + generate a
///    fresh `server_nonce`; write [`Response::HandshakeChallenge`].
/// 3. Read next frame, expect [`Request::HandshakeComplete`],
///    verify `client_signature` against `client_pubkey` over
///    `server_nonce`.
/// 4. Write final [`Response::Ack`] on success.
///
/// Any deviation from this sequence (wrong pubkey, bad signature,
/// wrong frame kind) returns `Err(reason)`; the caller surfaces the
/// reason inside a `Response::Error` and closes the connection.
async fn process_handshake<S>(
    stream: &mut S,
    identity: &NodeIdentity,
    client_pubkey: &[u8; 32],
    client_nonce: &[u8; 32],
    client_whitelist: Option<&crate::whitelist::Whitelist>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::identity::{fresh_nonce, verify_challenge};

    // Step 1: whitelist check (skipped in permissive mode).
    if let Some(wl) = client_whitelist {
        if wl.lookup_by_pubkey(client_pubkey).is_none() {
            return Err("client pubkey is not in ingress whitelist".into());
        }
    }

    // Step 2: sign client nonce + emit our own.
    let node_signature = identity.sign_challenge(client_nonce);
    let server_nonce = fresh_nonce();
    write_frame(
        stream,
        &Response::HandshakeChallenge {
            node_signature,
            server_nonce,
        }
        .encode(),
    )
    .await
    .map_err(|e| format!("write challenge: {e}"))?;

    // Step 3: read complete + verify.
    let buf = read_frame(stream)
        .await
        .map_err(|e| format!("read complete: {e}"))?;
    let req = Request::decode(&buf).map_err(|e| format!("decode complete: {e}"))?;
    let client_signature = match req {
        Request::HandshakeComplete { client_signature } => client_signature,
        other => {
            return Err(format!(
                "expected HandshakeComplete, got {:?}",
                std::mem::discriminant(&other)
            ));
        }
    };
    if !verify_challenge(client_pubkey, &server_nonce, &client_signature) {
        return Err("client signature over server nonce failed verification".into());
    }

    // Step 4: final Ack.
    write_frame(stream, &Response::Ack.encode())
        .await
        .map_err(|e| format!("write ack: {e}"))?;
    Ok(())
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
            match s.purge(object_id) {
                Ok(_) => Response::Ack,
                Err(e) => Response::Error(format!("purge failed: {e}")),
            }
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
            match s.purge_by_hashes(&set) {
                Ok(_) => Response::Ack,
                Err(e) => Response::Error(format!("purge_by_hash failed: {e}")),
            }
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
            match s.purge_by_hashes_up_to(&set, max_epoch) {
                Ok(_) => Response::Ack,
                Err(e) => Response::Error(format!("purge_by_hash_up_to failed: {e}")),
            }
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
        // Handshake frames are consumed by `handle_connection`
        // directly (they own the next round-trip on the wire, so
        // routing them through the generic request loop wouldn't
        // give us a place to write `HandshakeChallenge` before the
        // next read). Reaching here indicates a bug in the caller.
        Request::Handshake { .. } | Request::HandshakeComplete { .. } => {
            Response::Error(
                "handshake frames must not reach handle_request; dispatched via handle_connection"
                    .into(),
            )
        }
        Request::Capacity => {
            // P1.4b — take a short snapshot under the store lock (dir
            // path + live-bytes estimate) so we don't block the
            // filesystem call behind concurrent PUTs. `statvfs` is
            // ~microseconds on Linux, but we still don't want to hold
            // the store mutex across it.
            let (dir, live_bytes) = {
                let s = store.lock().await;
                (s.dir().map(std::path::PathBuf::from), s.live_bytes_estimate())
            };
            let (free_bytes, total_bytes) = match dir.as_deref() {
                Some(d) => crate::disk_space::free_and_total_bytes(d),
                None => (0, 0),
            };
            Response::Capacity {
                free_bytes,
                total_bytes,
                live_bytes,
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
        let removed = s.purge_by_hashes_up_to(&targets, snapshot).unwrap();
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

    #[test]
    fn compact_produces_compacted_segment_and_gcs_regulars() {
        // Direct call to `Store::compact` (no background loop). Put
        // a mix of live + tombstoned shards, then compact. Assert
        // that: (a) wal-c<epoch>.seg exists, (b) all wal-N.seg with
        // N ≤ epoch are gone, (c) reopening the Store recovers the
        // same live state.
        let dir = tmpdir("compact-basic");
        let mut store = Store::open(&dir).unwrap();
        // 3 live puts + 1 purge of one of them.
        store
            .put_appended((1, 0, 0), make_shard(1));
        store
            .put_appended((2, 0, 0), make_shard(2));
        store
            .put_appended((3, 0, 0), make_shard(3));
        store.purge(2).unwrap();
        // At this point: shards for object 2 tombstoned; 1 and 3 live.
        assert_eq!(store.total(), 2);

        let report = store.compact().unwrap().expect("should have compacted");
        assert!(report.records >= 2, "at least the 2 live shards; got {}", report.records);
        assert!(report.compact_path.file_name().unwrap()
            .to_string_lossy()
            .starts_with("wal-c"));

        // After compaction: wal-c<epoch>.seg is present, older
        // wal-N.seg files ≤ epoch are gone.
        let compacts = crate::wal::walk_compact_segments(&dir).unwrap();
        assert_eq!(compacts.len(), 1);
        assert_eq!(compacts[0].0, report.epoch);
        let regulars = crate::wal::walk_wal_segments(&dir).unwrap();
        for (seq, _) in &regulars {
            assert!(*seq > report.epoch, "regular seg {seq} should have been gc'd");
        }

        // Reopen: same live state.
        drop(store);
        let store2 = Store::open(&dir).unwrap();
        assert_eq!(store2.total(), 2);
        assert!(!store2.get((1, 0, 0)).is_empty());
        assert!(store2.get((2, 0, 0)).is_empty(), "purged obj must stay purged");
        assert!(!store2.get((3, 0, 0)).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_then_more_writes_replay_correctly() {
        // Compact, then write additional records into the fresh
        // active segment, then reopen. The compacted segment
        // provides the base state and the newer regular segment
        // applies deltas on top.
        let dir = tmpdir("compact-plus-delta");
        let mut store = Store::open(&dir).unwrap();
        store.put_appended((10, 0, 0), make_shard(10));
        store.put_appended((20, 0, 0), make_shard(20));
        let report = store.compact().unwrap().expect("should compact");
        let epoch = report.epoch;

        // Post-compact writes land in wal-<epoch+1>.seg (or later).
        store.put_appended((30, 0, 0), make_shard(30));
        store.purge(10).unwrap();

        drop(store);
        let store2 = Store::open(&dir).unwrap();
        // Expected live: obj 20 (from compacted), obj 30 (from
        // post-compact wal). obj 10 was purged after compaction.
        assert!(store2.get((10, 0, 0)).is_empty());
        assert!(!store2.get((20, 0, 0)).is_empty());
        assert!(!store2.get((30, 0, 0)).is_empty());

        // Sanity: any surviving regular segs have seq > epoch.
        for (seq, _) in crate::wal::walk_wal_segments(&dir).unwrap() {
            assert!(seq > epoch, "expected seq > {epoch}, saw {seq}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crash_after_compact_before_gc_still_replays_once() {
        // Simulate a crash between step 3 (compact seg durably
        // written) and step 4 (gc of old wal-N.seg). Old regulars
        // are still on disk alongside wal-c<epoch>.seg; boot must
        // skip them so records aren't double-applied.
        let dir = tmpdir("compact-crash-pre-gc");
        let mut store = Store::open(&dir).unwrap();
        store.put_appended((1, 0, 0), make_shard(1));
        store.put_appended((2, 0, 0), make_shard(2));

        // Manually invoke the primitives so we can stop between
        // steps 3 and 4 (skip gc_compacted).
        let dir_clone = dir.clone();
        {
            let wal = store.wal.as_mut().unwrap();
            wal.sync(true).unwrap();
            let epoch = wal.active_seq();
            wal.rotate().unwrap();
            let records: Vec<_> = store.shards.iter().flat_map(|((o, c, l), bucket)| {
                let o = *o; let c = *c; let l = *l;
                bucket.iter().map(move |(h, (s, _))| (o, c, l, *h, s.clone()))
            }).collect();
            crate::wal::write_compacted_segment(&dir_clone, epoch, records, None).unwrap();
            // Deliberately skip `wal::gc_compacted(&dir, epoch)`.
        }

        // Sanity: both the compact seg AND the old wal-N.seg files
        // are on disk simultaneously.
        assert_eq!(crate::wal::walk_compact_segments(&dir).unwrap().len(), 1);
        assert!(crate::wal::walk_wal_segments(&dir).unwrap().len() >= 1);

        drop(store);
        let store2 = Store::open(&dir).unwrap();
        // Live state must equal what it was before the "crash",
        // not doubled.
        assert_eq!(store2.total(), 2);
        assert_eq!(store2.get((1, 0, 0)).len(), 1);
        assert_eq!(store2.get((2, 0, 0)).len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_no_op_on_fresh_store() {
        // Freshly-opened store has one empty segment (magic only);
        // compaction should be a no-op (returns Ok(None)) rather
        // than churning out empty compact segs on every tick.
        let dir = tmpdir("compact-noop");
        let mut store = Store::open(&dir).unwrap();
        let report = store.compact().unwrap();
        assert!(report.is_none(), "should skip: got {report:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_no_op_on_in_memory_store() {
        // Store::new() has no dir + no wal — compaction is
        // impossible, must return Ok(None), not error.
        let mut store = Store::new();
        assert!(store.compact().unwrap().is_none());
    }

    /// Framed request/response over any `AsyncRead + AsyncWrite`
    /// stream — needed for the TLS path where the stream isn't a
    /// bare TcpStream. Wire framing matches `rpc` above.
    async fn rpc_generic<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        stream: &mut S,
        req: Request,
    ) -> Response {
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
    async fn mtls_node_accepts_client_with_valid_cert() {
        // Full P0.3-flavored end-to-end: generate self-signed
        // TlsMaterial, spawn a persistent node with the resulting
        // ServerConfig (mtls = true so the node also requires a
        // client cert), connect from a TLS client presenting the
        // same-CA client cert, and do a round-trip Ping. Verifies
        // that the CLI wiring path — `--tls-cert/--tls-key/--tls-ca
        // --mtls` → `TlsMaterial::server_config(true)` →
        // `spawn_node_persistent_with_tls` — actually terminates
        // TLS at the node.
        use crate::tls::{server_name_for, TlsMaterial};
        use tokio_rustls::TlsConnector;

        let (material, _signer) =
            TlsMaterial::self_signed("test-node", &["127.0.0.1".into()]).unwrap();
        let server_cfg = material.server_config(true).unwrap();
        let client_cfg = material.client_config(true).unwrap();
        let client_cfg_bad = TlsMaterial::self_signed("intruder", &["127.0.0.1".into()])
            .unwrap()
            .0
            .client_config(true)
            .unwrap();

        let dir = tmpdir("mtls-node");
        let (addr, _store, _h) = spawn_node_persistent_with_tls(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            Some(server_cfg),
        )
        .await
        .unwrap();

        // Happy path: same-CA client cert accepted, wire RPC succeeds.
        let tcp = TcpStream::connect(addr).await.unwrap();
        let connector = TlsConnector::from(client_cfg);
        let sni = server_name_for(&format!("{}", addr.ip())).unwrap();
        let mut tls_stream = connector.connect(sni, tcp).await.unwrap();
        assert_eq!(rpc_generic(&mut tls_stream, Request::Ping).await, Response::Pong);

        // Wrong-CA client cert: TLS handshake refused before any
        // wire byte lands. Node rejecting → connect returns Err.
        // (rustls surfaces this as a `bad_certificate` alert.)
        let tcp2 = TcpStream::connect(addr).await.unwrap();
        let connector_bad = TlsConnector::from(client_cfg_bad);
        let sni2 = server_name_for(&format!("{}", addr.ip())).unwrap();
        let handshake_res = connector_bad.connect(sni2, tcp2).await;
        assert!(
            handshake_res.is_err(),
            "mtls node must reject client cert signed by a different CA"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // === P0.3b bilateral handshake + ingress whitelist ================

    /// Build a signed whitelist containing `authorised_clients` — one
    /// WhitelistEntry per pubkey. `addr` is empty (clients are
    /// address-agnostic; the field is only meaningful for node entries).
    fn client_whitelist(
        admin: &NodeIdentity,
        authorised_clients: &[crate::identity::PubKey],
    ) -> Arc<crate::whitelist::Whitelist> {
        let entries: Vec<crate::whitelist::WhitelistEntry> = authorised_clients
            .iter()
            .map(|pk| crate::whitelist::WhitelistEntry {
                addr: String::new(),
                pubkey: *pk,
                zone: 0,
            })
            .collect();
        Arc::new(crate::whitelist::Whitelist::sign(entries, admin))
    }

    #[tokio::test]
    async fn permissive_mode_accepts_peers_without_handshake() {
        // No client_whitelist configured → pre-P0.3b behaviour: any
        // TCP peer can Ping without touching the handshake flow.
        // Guards against a regression that would silently require
        // handshake for legacy clients.
        let dir = tmpdir("permissive-mode");
        let (addr, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None, // no TLS
            None, // no whitelist → permissive
        )
        .await
        .unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(rpc(&mut s, Request::Ping).await, Response::Pong);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn strict_mode_rejects_first_non_handshake_frame() {
        // Whitelist configured but client speaks Put directly → node
        // MUST respond with Error and close the connection before
        // touching the store.
        let dir = tmpdir("strict-rejects-put");
        let admin = NodeIdentity::generate();
        let authorised = NodeIdentity::generate();
        let wl = client_whitelist(&admin, &[authorised.pubkey()]);
        let (addr, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(wl),
        )
        .await
        .unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        match rpc(&mut s, Request::Ping).await {
            Response::Error(msg) => {
                assert!(msg.contains("handshake required"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // Next read should hit EOF — connection closed.
        let mut buf = [0u8; 4];
        let n = s.read(&mut buf).await.unwrap_or(0);
        assert_eq!(n, 0, "connection must be closed after strict rejection");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn handshake_from_authorised_client_unlocks_rpc() {
        // Full bilateral flow via the client helper: after successful
        // handshake, the connection accepts Put/Get like normal.
        let dir = tmpdir("handshake-happy");
        let admin = NodeIdentity::generate();
        let client_id = NodeIdentity::generate();
        let wl = client_whitelist(&admin, &[client_id.pubkey()]);
        let (addr, store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(wl),
        )
        .await
        .unwrap();
        // Node identity is what the persistent-mode loader created
        // under storage_dir/identity.key.
        let node_id =
            crate::identity::NodeIdentity::load_or_create(dir.join("identity.key")).unwrap();
        let node_pubkey = node_id.pubkey();

        let mut s = TcpStream::connect(addr).await.unwrap();
        perform_bilateral_handshake_as_client(&mut s, &client_id, &node_pubkey)
            .await
            .expect("handshake should succeed for authorised client");

        // Post-handshake, normal RPC works.
        assert_eq!(rpc(&mut s, Request::Ping).await, Response::Pong);
        let sh = make_shard(1);
        rpc(
            &mut s,
            Request::Put {
                object_id: 42,
                channel: 0,
                layer: 0,
                shard: sh.clone(),
            },
        )
        .await;
        // Assert against the store, not just the response, so we
        // prove the write actually landed.
        let stored = store.lock().await.get((42, 0, 0));
        assert_eq!(stored.len(), 1);
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn handshake_from_unauthorised_client_rejected() {
        // Client presents a valid Ed25519 identity that isn't on the
        // node's whitelist. Node must reject at the whitelist-check
        // step (before generating any signatures/nonces) and close
        // the connection.
        let dir = tmpdir("handshake-unauth");
        let admin = NodeIdentity::generate();
        let authorised = NodeIdentity::generate();
        let intruder = NodeIdentity::generate();
        let wl = client_whitelist(&admin, &[authorised.pubkey()]); // intruder NOT included
        let (addr, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(wl),
        )
        .await
        .unwrap();
        let node_id =
            crate::identity::NodeIdentity::load_or_create(dir.join("identity.key")).unwrap();
        let node_pubkey = node_id.pubkey();

        let mut s = TcpStream::connect(addr).await.unwrap();
        let err = perform_bilateral_handshake_as_client(&mut s, &intruder, &node_pubkey)
            .await
            .expect_err("intruder must be rejected");
        assert!(
            err.contains("not in ingress whitelist") || err.contains("rejected"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn handshake_client_detects_node_signature_forgery() {
        // Symmetry test: if the client's expected_node_pubkey does
        // not match the actual node identity, the node's signature
        // over client_nonce will fail verification — proves the
        // node-side identity check is genuine, not accidentally
        // permissive.
        let dir = tmpdir("handshake-wrong-node-pk");
        let admin = NodeIdentity::generate();
        let client_id = NodeIdentity::generate();
        let wl = client_whitelist(&admin, &[client_id.pubkey()]);
        let (addr, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(wl),
        )
        .await
        .unwrap();
        // Pretend the client was told the node's pubkey is something
        // else (attacker-injected whitelist).
        let wrong_pubkey = NodeIdentity::generate().pubkey();

        let mut s = TcpStream::connect(addr).await.unwrap();
        let err = perform_bilateral_handshake_as_client(&mut s, &client_id, &wrong_pubkey)
            .await
            .expect_err("client must detect the signature mismatch");
        assert!(
            err.contains("verification"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn handshake_complete_without_handshake_is_rejected() {
        // Protocol-violation guard: a client that jumps straight to
        // HandshakeComplete (no preceding Handshake) must be closed
        // out. Prevents an attacker from trying to skip the
        // whitelist check by malformed sequencing.
        let dir = tmpdir("hc-out-of-sequence");
        let admin = NodeIdentity::generate();
        let ok_client = NodeIdentity::generate();
        let wl = client_whitelist(&admin, &[ok_client.pubkey()]);
        let (addr, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(wl),
        )
        .await
        .unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        match rpc(
            &mut s,
            Request::HandshakeComplete {
                client_signature: [0u8; 64],
            },
        )
        .await
        {
            Response::Error(msg) => assert!(msg.contains("unexpected"), "got: {msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- P1.4b Capacity handler ---

    #[tokio::test]
    async fn capacity_on_in_memory_node_returns_unknown_sentinel() {
        // In-memory `Store::new()` has no `dir()`; the handler must
        // fold that into the `(0, 0)` unknown-capacity contract while
        // still reporting `live_bytes = 0` (nothing stored yet).
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        let resp = rpc(&mut s, Request::Capacity).await;
        assert_eq!(
            resp,
            Response::Capacity {
                free_bytes: 0,
                total_bytes: 0,
                live_bytes: 0,
            },
            "in-memory node must report the (0, 0, 0) unknown-capacity sentinel"
        );
    }

    #[tokio::test]
    async fn capacity_on_persistent_node_reports_real_disk_and_live_bytes() {
        let dir = tmpdir("capacity-persistent");
        let (addr, _store, _h) =
            spawn_node_persistent((Ipv4Addr::LOCALHOST, 0).into(), &dir)
                .await
                .unwrap();

        // Baseline: fresh node, no shards → live_bytes=0 but the mount
        // should report a real total capacity via statvfs.
        let mut s = TcpStream::connect(addr).await.unwrap();
        let baseline = rpc(&mut s, Request::Capacity).await;
        let (baseline_free, baseline_total, baseline_live) = match baseline {
            Response::Capacity {
                free_bytes,
                total_bytes,
                live_bytes,
            } => (free_bytes, total_bytes, live_bytes),
            other => panic!("expected Capacity, got {other:?}"),
        };
        assert!(
            baseline_total > 0,
            "persistent node on a real fs should report nonzero total_bytes"
        );
        assert!(
            baseline_free <= baseline_total,
            "free_bytes ({baseline_free}) must not exceed total_bytes ({baseline_total})"
        );
        assert_eq!(baseline_live, 0, "no shards yet → live_bytes should be 0");

        // After a Put: live_bytes must grow by at least the shard's
        // in-RAM footprint. This is the observable signal the gateway
        // uses to detect where data actually lives.
        let sh = make_shard(11);
        let expected_growth = (sh.coeffs.len() + sh.payload.len()) as u64;
        rpc(
            &mut s,
            Request::Put {
                object_id: 42,
                channel: 0,
                layer: 0,
                shard: sh,
            },
        )
        .await;

        let after = rpc(&mut s, Request::Capacity).await;
        let after_live = match after {
            Response::Capacity { live_bytes, .. } => live_bytes,
            other => panic!("expected Capacity, got {other:?}"),
        };
        assert!(
            after_live >= expected_growth,
            "live_bytes ({after_live}) should have grown by at least {expected_growth} after the Put"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
