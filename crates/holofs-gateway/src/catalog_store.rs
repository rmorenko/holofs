//! Redb-backed persistence for the in-memory [`Directory`].
//!
//! Motivation (PRODUCTION-READINESS §P0.1c). The previous
//! implementation re-encoded the entire `Directory` on every flush
//! and wrote it to `catalog.holo` via
//! [`holofs_model::fs::write_atomic`]. That was O(M) per flush plus
//! a single-file corruption point — a torn write on the atomic
//! rename could lose the whole catalog. The group-commit ticket
//! dance in [`Gateway::persist_catalog`] amortised the flush
//! across concurrent mutations but did not shrink the per-flush
//! work.
//!
//! This module wraps [`redb`] as a transactional KV: one table
//! `entries` keyed by catalog name, values are the wire encoding
//! of [`Manifest`]. Each catalog mutation touches exactly the
//! keys it changes (via [`Self::insert_many`] / [`Self::remove_many`])
//! inside a single write transaction that commits with
//! `Durability::Immediate` — fsync is per-flush, not per-key. Reads
//! still hit the in-memory `Directory` for O(1) latency; the redb
//! store is only used to survive restart.
//!
//! ## Migration
//!
//! [`migrate_legacy_if_present`] is idempotent: on first boot after
//! upgrade it reads any existing `catalog.holo`, replays every
//! entry into a fresh `catalog.redb`, and renames the legacy file
//! to `catalog.holo.migrated-<unix_secs>` as a safety backup
//! (not deleted so operators can verify the migration or roll back
//! manually if needed). Subsequent boots see `catalog.redb` already
//! present and skip the pass.
//!
//! ## Crash safety
//!
//! redb's own commit protocol guarantees the write-txn is atomic:
//! either all inserts/removes in the batch land or none do. The
//! catalog cannot end up in a half-flushed state, which was the
//! risk the whole-file rewrite carried when a torn rename hit
//! mid-fsync.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, Durability, ReadableTable, TableDefinition};

use holofs_model::fs::Directory;
use holofs_model::manifest::Manifest;

/// One table for the whole catalog. Keys are catalog names
/// (e.g. `photos/2026/beach.jpg`), values are the
/// [`Manifest::encode`] byte stream.
const ENTRIES: TableDefinition<&str, &[u8]> = TableDefinition::new("entries");

/// On-disk filename for the redb database.
pub const CATALOG_DB_FILENAME: &str = "catalog.redb";
/// On-disk filename for the legacy whole-file catalog encoding.
/// Present only on installs that have not yet been migrated;
/// [`migrate_legacy_if_present`] renames it out of the way after a
/// successful migration.
pub const CATALOG_LEGACY_FILENAME: &str = "catalog.holo";

/// Thin wrapper around a [`redb::Database`] that exposes exactly the
/// operations the gateway's persistence path needs. All fallible
/// methods surface [`io::Error`] so they slot into the existing
/// `Gateway::persist_catalog` error plumbing without a new error
/// enum.
pub struct CatalogStore {
    db: Database,
    path: PathBuf,
}

impl CatalogStore {
    /// Open (creating if necessary) `<dir>/catalog.redb`. The parent
    /// directory must already exist.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join(CATALOG_DB_FILENAME);
        let db = Database::create(&path).map_err(redb_err)?;
        Ok(Self { db, path })
    }

    /// Full on-disk path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replay every persisted entry into `catalog`. Called once at
    /// boot after [`Self::open`]. Overwrites any existing entries in
    /// `catalog` with the same key. Returns the number of entries
    /// loaded.
    ///
    /// Fails only on redb internal errors; a decode error on a
    /// single manifest is surfaced (we don't silently skip because
    /// that would drop objects the user believes are safe).
    pub fn load_into(&self, catalog: &mut Directory) -> io::Result<usize> {
        let read_txn = self.db.begin_read().map_err(redb_err)?;
        let table = match read_txn.open_table(ENTRIES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(redb_err(e)),
        };
        let mut count = 0usize;
        let iter = table.iter().map_err(redb_err)?;
        for entry in iter {
            let (k, v) = entry.map_err(redb_err)?;
            let name = k.value().to_string();
            let bytes = v.value();
            let manifest = Manifest::decode(bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("catalog.redb: decode of entry {name:?} failed: {e}"),
                )
            })?;
            catalog.insert(name, manifest);
            count += 1;
        }
        Ok(count)
    }

    /// Persist a batch of upserts + removes atomically. Either every
    /// change in the batch lands or none does. `upserts` supplies
    /// pre-encoded manifest bytes (caller already knows how, and it
    /// keeps the CatalogStore from depending on the Manifest encoder
    /// beyond the boot-time decode above).
    ///
    /// Commits with `Durability::Immediate` so the fsync happens
    /// before this returns — mirrors the pre-refactor
    /// `write_atomic` contract.
    pub fn apply_batch(
        &self,
        upserts: impl IntoIterator<Item = (String, Vec<u8>)>,
        removes: impl IntoIterator<Item = String>,
    ) -> io::Result<()> {
        let mut write_txn = self.db.begin_write().map_err(redb_err)?;
        write_txn.set_durability(Durability::Immediate);
        {
            let mut table = write_txn.open_table(ENTRIES).map_err(redb_err)?;
            for (name, bytes) in upserts {
                table
                    .insert(name.as_str(), bytes.as_slice())
                    .map_err(redb_err)?;
            }
            for name in removes {
                table.remove(name.as_str()).map_err(redb_err)?;
            }
        }
        write_txn.commit().map_err(redb_err)?;
        Ok(())
    }
}

/// One-shot boot migration: if `<dir>/catalog.redb` does not exist
/// but `<dir>/catalog.holo` does, replay every entry from the legacy
/// wire encoding into a fresh redb, then rename the legacy file to
/// `catalog.holo.migrated-<unix_secs>` as a safety backup.
///
/// Returns:
/// - `Ok(None)` if either the redb already exists (already migrated)
///   or the legacy file is absent (fresh install).
/// - `Ok(Some(backup_path))` on a completed migration; `backup_path`
///   is the rename target. Operators can delete it after verifying
///   the redb reads correctly.
///
/// Called from `Gateway::open` before `CatalogStore::open` +
/// `load_into` so subsequent boots take the same code path as fresh
/// installs.
pub fn migrate_legacy_if_present(dir: &Path) -> io::Result<Option<PathBuf>> {
    let redb_path = dir.join(CATALOG_DB_FILENAME);
    let legacy_path = dir.join(CATALOG_LEGACY_FILENAME);
    if redb_path.exists() {
        return Ok(None);
    }
    if !legacy_path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&legacy_path)?;
    let directory = Directory::decode(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "catalog.holo migration: decode failed ({e}); refusing to touch \
                 catalog — inspect the file manually before retrying"
            ),
        )
    })?;

    let store = CatalogStore::open(dir)?;
    let upserts: Vec<(String, Vec<u8>)> = directory
        .entries
        .iter()
        .map(|(name, manifest)| (name.clone(), manifest.encode()))
        .collect();
    let count = upserts.len();
    store.apply_batch(upserts, std::iter::empty())?;

    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_path = dir.join(format!("{CATALOG_LEGACY_FILENAME}.migrated-{unix_secs}"));
    std::fs::rename(&legacy_path, &backup_path)?;

    eprintln!(
        "catalog: migrated {count} entries from {} → {} (legacy backed up at {})",
        legacy_path.display(),
        store.path().display(),
        backup_path.display(),
    );

    Ok(Some(backup_path))
}

fn redb_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("catalog.redb: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use holofs_model::manifest::{ManifestState, ObjectEncoding, ObjectKind};
    use holofs_model::placement::Placement;
    use tempfile::TempDir;

    fn mk_manifest(seed: u8) -> Manifest {
        Manifest {
            object_id: seed as u64,
            k: 16,
            nlayers: 2,
            n_per_layer: vec![32, 24],
            sym_len: vec![64, 128],
            layer_positions: vec![vec![0, 1, 2], vec![3, 4, 5]],
            channels: 3,
            width: 16,
            height: 16,
            levels: 1,
            nodes: vec!["127.0.0.1:5000".into(), "127.0.0.1:5001".into()],
            placement: Placement::Rendezvous,
            zones: vec![0; 2],
            data_cid: [seed; 32],
            merkle_root: [seed.wrapping_add(1); 32],
            shard_hashes: vec![vec![Vec::new(); 2]; 3],
            kind: ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 1_700_000_000 + seed as u64,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
        }
    }

    #[test]
    fn open_creates_empty_db_and_load_into_returns_zero() {
        let dir = TempDir::new().unwrap();
        let store = CatalogStore::open(dir.path()).unwrap();
        assert!(store.path().ends_with(CATALOG_DB_FILENAME));
        let mut cat = Directory::new();
        assert_eq!(store.load_into(&mut cat).unwrap(), 0);
        assert!(cat.entries.is_empty());
    }

    #[test]
    fn apply_batch_upserts_and_removes_survive_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let store = CatalogStore::open(dir.path()).unwrap();
            store
                .apply_batch(
                    vec![
                        ("a.txt".to_string(), mk_manifest(1).encode()),
                        ("b/c.txt".to_string(), mk_manifest(2).encode()),
                        ("stale.txt".to_string(), mk_manifest(9).encode()),
                    ],
                    std::iter::empty(),
                )
                .unwrap();
            store
                .apply_batch(std::iter::empty(), vec!["stale.txt".to_string()])
                .unwrap();
            // Close/drop so we don't hold a file lock during reopen.
        }
        // Fresh handle on the same dir, same data.
        let store = CatalogStore::open(dir.path()).unwrap();
        let mut cat = Directory::new();
        let n = store.load_into(&mut cat).unwrap();
        assert_eq!(n, 2);
        assert!(cat.get("a.txt").is_some());
        assert!(cat.get("b/c.txt").is_some());
        assert!(cat.get("stale.txt").is_none());
    }

    #[test]
    fn apply_batch_atomicity_upserts_and_removes_in_one_txn() {
        // Both the upsert and the remove commit together. Between
        // batches, a reader shouldn't observe an intermediate state
        // (this is a redb correctness property, but exercising it
        // gives us a concrete test of the batching API shape).
        let dir = TempDir::new().unwrap();
        let store = CatalogStore::open(dir.path()).unwrap();
        store
            .apply_batch(
                vec![("keep.txt".to_string(), mk_manifest(1).encode())],
                std::iter::empty(),
            )
            .unwrap();
        store
            .apply_batch(
                vec![("new.txt".to_string(), mk_manifest(2).encode())],
                vec!["keep.txt".to_string()],
            )
            .unwrap();

        drop(store);
        let store = CatalogStore::open(dir.path()).unwrap();
        let mut cat = Directory::new();
        store.load_into(&mut cat).unwrap();
        assert_eq!(cat.entries.len(), 1);
        assert!(cat.get("new.txt").is_some());
        assert!(cat.get("keep.txt").is_none());
    }

    #[test]
    fn migrate_legacy_populates_redb_and_backs_up_holo() {
        let dir = TempDir::new().unwrap();
        // Build a legacy catalog.holo containing 3 entries.
        let mut legacy = Directory::new();
        legacy.insert("a.txt".to_string(), mk_manifest(1));
        legacy.insert("b/c.txt".to_string(), mk_manifest(2));
        legacy.insert("d/e/f.jpg".to_string(), mk_manifest(3));
        std::fs::write(dir.path().join(CATALOG_LEGACY_FILENAME), legacy.encode()).unwrap();

        let backup = migrate_legacy_if_present(dir.path()).unwrap();
        let backup = backup.expect("migration should have run");
        assert!(backup.file_name().unwrap().to_string_lossy().starts_with(
            &format!("{CATALOG_LEGACY_FILENAME}.migrated-")
        ));
        assert!(!dir.path().join(CATALOG_LEGACY_FILENAME).exists(), "legacy file should be renamed");
        assert!(dir.path().join(CATALOG_DB_FILENAME).exists(), "redb should be created");

        let store = CatalogStore::open(dir.path()).unwrap();
        let mut cat = Directory::new();
        assert_eq!(store.load_into(&mut cat).unwrap(), 3);
        assert!(cat.get("a.txt").is_some());
        assert!(cat.get("b/c.txt").is_some());
        assert!(cat.get("d/e/f.jpg").is_some());
    }

    #[test]
    fn migrate_is_noop_when_redb_already_present() {
        let dir = TempDir::new().unwrap();
        // Simulate a prior migration: redb exists with one entry,
        // and a legacy .holo file is still lying around from a
        // sloppy manual restore.
        {
            let store = CatalogStore::open(dir.path()).unwrap();
            store
                .apply_batch(
                    vec![("pre-existing.txt".to_string(), mk_manifest(7).encode())],
                    std::iter::empty(),
                )
                .unwrap();
        }
        let mut ghost = Directory::new();
        ghost.insert("should-be-ignored.txt".to_string(), mk_manifest(99));
        std::fs::write(dir.path().join(CATALOG_LEGACY_FILENAME), ghost.encode()).unwrap();

        let result = migrate_legacy_if_present(dir.path()).unwrap();
        assert!(result.is_none(), "should skip: redb is authoritative");
        // Legacy file untouched (operator's problem to clean up).
        assert!(dir.path().join(CATALOG_LEGACY_FILENAME).exists());
        // redb still holds only what was there before.
        let store = CatalogStore::open(dir.path()).unwrap();
        let mut cat = Directory::new();
        store.load_into(&mut cat).unwrap();
        assert_eq!(cat.entries.len(), 1);
        assert!(cat.get("pre-existing.txt").is_some());
        assert!(cat.get("should-be-ignored.txt").is_none());
    }

    #[test]
    fn migrate_is_noop_on_fresh_install() {
        let dir = TempDir::new().unwrap();
        let result = migrate_legacy_if_present(dir.path()).unwrap();
        assert!(result.is_none());
        assert!(!dir.path().join(CATALOG_DB_FILENAME).exists());
    }
}
