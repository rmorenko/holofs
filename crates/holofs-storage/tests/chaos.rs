//! P1.6 chaos integration tests (feature-gated).
//!
//! These tests exercise the `chaos::maybe_fail_wal_{append,sync}`
//! fault-injection hooks wired into `WalWriter`. They validate two
//! shapes of failure a real cluster can hit but a normal filesystem
//! test can't easily trigger:
//!
//! 1. **Disk full** during WAL append — the store must refuse the
//!    write cleanly (no RAM index entry, no on-disk half-record) and
//!    survive a subsequent successful append after the fault clears.
//! 2. **Mid-segment bit-flip** in a closed WAL segment — the boot
//!    replay (`Store::open`) must surface `InvalidData` rather than
//!    silently drop the corrupted-then-truncated tail.
//!
//! The chaos module is compiled out when the `chaos` crate feature is
//! off; this whole file lives behind `#[cfg(feature = "chaos")]` so
//! the default `cargo test -p holofs-storage` run skips it. CI adds
//! a dedicated `cargo test -p holofs-storage --features chaos` step
//! (see `.github/workflows/ci.yml`).

#![cfg(feature = "chaos")]

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::sync::Mutex;

use holofs_core::rlnc::Shard;
use holofs_storage::chaos;
use holofs_storage::node_service::Store;
use tempfile::TempDir;

/// The chaos fault registers are process-global atomics; cargo test
/// runs tests within one binary in parallel by default, so two chaos
/// tests can race on the register unless serialised. Every test in
/// this file takes this guard, which also disarms all faults on
/// acquire so a panicked earlier test can't leak a fault into ours.
static CHAOS_LOCK: Mutex<()> = Mutex::new(());

fn chaos_guard() -> std::sync::MutexGuard<'static, ()> {
    let g = CHAOS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    chaos::disarm_all();
    g
}

fn tiny_shard(byte: u8) -> Shard {
    Shard {
        coeffs: vec![byte, byte, byte],
        payload: vec![byte; 32],
    }
}

#[test]
fn disk_full_on_wal_append_refuses_put_cleanly() {
    let _guard = chaos_guard();
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();

    // Baseline write succeeds — WAL is healthy, RAM index picks it up.
    let ok = store.put((1, 0, 0), tiny_shard(0xAA));
    assert!(ok, "baseline put should succeed");
    assert_eq!(store.get((1, 0, 0)).len(), 1);

    // Arm one-shot disk-full for the next WAL append.
    chaos::arm_wal_next_append_fails(ErrorKind::StorageFull);
    let ok = store.put((1, 0, 0), tiny_shard(0xBB));
    // The failed WAL append surfaces to `put` as `false` (same shape
    // as duplicate) and — critically — the RAM index is NOT updated.
    assert!(!ok, "disk-full injection must refuse the put");
    assert_eq!(
        store.get((1, 0, 0)).len(),
        1,
        "RAM index must not gain a shard when WAL append refused it"
    );

    // Next put must succeed — the fault was one-shot.
    let ok = store.put((1, 0, 0), tiny_shard(0xCC));
    assert!(ok, "post-fault put should recover");
    assert_eq!(store.get((1, 0, 0)).len(), 2);
}

#[test]
fn disk_full_on_wal_append_leaves_no_corruption_on_disk() {
    // Committed-then-failed-then-committed sequence. Reopening the
    // store from the same directory must see exactly the committed
    // records — the failed append neither corrupts the log nor
    // leaves a phantom entry.
    let _guard = chaos_guard();
    let dir = TempDir::new().unwrap();
    {
        let mut store = Store::open(dir.path()).unwrap();
        assert!(store.put((10, 0, 0), tiny_shard(0xAA)));
        chaos::arm_wal_next_append_fails(ErrorKind::StorageFull);
        assert!(!store.put((11, 0, 0), tiny_shard(0xBB)));
        assert!(store.put((12, 0, 0), tiny_shard(0xCC)));
    } // drop → flush buffered writer

    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.get((10, 0, 0)).len(), 1);
    assert_eq!(
        reopened.get((11, 0, 0)).len(),
        0,
        "the failed append must not survive across boot"
    );
    assert_eq!(reopened.get((12, 0, 0)).len(), 1);
}

#[test]
fn wal_bitflip_in_closed_segment_fails_store_open() {
    // Integration flavour of the wal-level bitflip test: exercise the
    // full `Store::open` boot path (not just `read_segment`) to prove
    // that a mid-segment bit-flip in a rotated WAL segment is
    // surfaced as an error at boot rather than silently dropping the
    // corrupted tail. This is the P1.6 chaos scenario "operator
    // discovers WAL corruption on restart" — the store must refuse
    // to boot on a suspicious log rather than come up with a partial
    // view of history.
    let _guard = chaos_guard();
    let dir = TempDir::new().unwrap();

    // Write a handful of shards, then let the store drop so the WAL
    // writer flushes its BufWriter before we touch the file.
    {
        let mut store = Store::open(dir.path()).unwrap();
        for i in 0..8u64 {
            assert!(store.put((i, 0, 0), tiny_shard(i as u8)));
        }
        // Force the active segment to close so a footer is written
        // — mid-segment bit-flip detection relies on the footer's
        // record count + body digest cross-check.
        store.compact().unwrap();
    }

    // Find any wal-*.seg or wal-c*.seg the store wrote and flip a
    // byte deep inside its record region (past the 8-byte segment
    // magic, well before the 20-byte footer). Prefer a compacted
    // segment if one exists — it's the file the boot path leans on
    // first.
    let seg_path = {
        let mut candidates: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n.starts_with("wal-") && n.ends_with(".seg"))
                    .unwrap_or(false)
            })
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .find(|p| {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                // Prefer a compacted segment (`wal-c*.seg`) since it
                // carries the largest record set after `compact()`.
                name.starts_with("wal-c")
            })
            .expect("compact() should have written a wal-c*.seg segment")
    };

    // Read → flip byte 40 (well inside the first record body) → write.
    let mut bytes = Vec::new();
    fs::File::open(&seg_path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let flip_at = 40;
    assert!(
        flip_at + 20 < bytes.len(),
        "test needs a segment large enough for a byte flip well before the footer"
    );
    bytes[flip_at] ^= 0x01;
    let mut f = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&seg_path)
        .unwrap();
    f.write_all(&bytes).unwrap();
    drop(f);

    // Boot must refuse the corrupted log — mid-segment corruption
    // is not silently truncated when the footer says more records
    // should be present. `Store` doesn't derive `Debug`, so we
    // extract the error via `err()` rather than `expect_err`.
    let err = Store::open(dir.path()).err().expect(
        "Store::open on a bit-flipped compacted segment should surface InvalidData, \
         not silently drop the tail",
    );
    assert_eq!(err.kind(), ErrorKind::InvalidData, "got: {err}");
}
