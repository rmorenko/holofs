//! file interface.
//!
//! - [`Directory`] — a `name → manifest` catalog. This is a "directory" in
//!   spec terms: a set of file entries, each with its own manifest (i.e. CID,
//!   size, placement scheme).
//! - Progressive reads live in `holofs_client::get_object_up_to_layer`:
//!   requesting only coarse layers yields an instant preview; finer layers
//!   are fetched on demand.
//!
//! External publication goes through the HTTP gateway (see
//! `holofs-gateway`). Cross-platform, no kext/FUSE; Range requests map to
//! byte ranges of the PNG, layer depth is chosen by the route
//! (`/preview/<name>` vs `/<name>`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use holofs_core::merkle::Hash;

use crate::manifest::{Manifest, ObjectKind};
use crate::path as catalog_path;

const MAGIC: &[u8; 8] = b"HOLOFSD1";

/// The gateway's in-memory catalog. Every mutation lands here and
/// then eventually gets flushed to disk by
/// [`crate::fs::Directory::save_atomic`].
///
/// Manifests are stored behind `Arc` so that:
///
/// * `Directory::clone()` is O(N) pointer bumps instead of O(N × M)
///   byte copies (where M is `manifest.shard_hashes.len()`).
///   `persist_catalog` clones the catalog every flush; before this
///   refactor a 500-entry catalog with fat `shard_hashes` cost ~5 MB
///   of allocation + memcpy per flush, and under a 50-worker soak
///   that contention against the same `Mutex<Directory>` gave the
///   `stats`/`spotlight`/`similar`/`versions_list` endpoints a
///   double-digit % 504 rate.
/// * Snapshots stay valid across concurrent mutations. Callers that
///   need to mutate an entry go through [`Self::get_mut`], which
///   uses `Arc::make_mut` — copy-on-write clones the inner
///   `Manifest` only when another reader (e.g. the persist snapshot)
///   is still holding a handle to the old value.
#[derive(Default, Clone, Debug)]
pub struct Directory {
    pub entries: BTreeMap<String, Arc<Manifest>>,
    /// v4 (batch-delete perf): reverse index `shard_hash → set of catalog
    /// names that reference it`. Kept in sync by
    /// [`Self::insert`] / [`Self::insert_arc`] / [`Self::remove`] and
    /// rebuilt from `entries` after [`Self::decode`]. Never serialised —
    /// deriving it on load costs O(N × shards) once but avoids a wire-
    /// format bump.
    ///
    /// Consumers use [`Self::orphan_hashes`] instead of scanning
    /// `entries` when deciding which shard hashes are safe to purge —
    /// turning `Gateway::purge_orphans_of` from O(catalog × shards) per
    /// call into O(shards) per call. That kills the O(N²) drain-25k
    /// behaviour caught by `holofs-stability`.
    shard_refs: HashMap<Hash, HashSet<String>>,
}

// The reverse index is derivable from `entries`, so hand-roll `PartialEq`
// to only compare `entries`. Otherwise, two catalogs with identical
// content but different HashMap iteration order would look unequal.
impl PartialEq for Directory {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}
impl Eq for Directory {}

impl Directory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, manifest: Manifest) {
        self.insert_arc(name, Arc::new(manifest));
    }

    /// Insert an already-shared manifest. Used by mv-style paths that
    /// want to move an existing catalog entry to a new key without
    /// deep-cloning the manifest first.
    ///
    /// Keeps `shard_refs` in sync: if `name` was already taken, drop
    /// the old manifest's references first; then add references from
    /// every shard hash in the new manifest.
    pub fn insert_arc(&mut self, name: String, manifest: Arc<Manifest>) {
        if let Some(prev) = self.entries.get(&name) {
            let prev = Arc::clone(prev);
            self.deindex(&name, &prev);
        }
        self.index(&name, &manifest);
        self.entries.insert(name, manifest);
    }

    /// Immutable-borrow lookup. Deref through the `Arc` so callers
    /// keep the pre-refactor `Option<&Manifest>` ergonomics.
    pub fn get(&self, name: &str) -> Option<&Manifest> {
        self.entries.get(name).map(|arc| arc.as_ref())
    }

    /// Copy-on-write mutable access. If the entry's `Arc` is uniquely
    /// held (typical hot path), returns a `&mut Manifest` into it
    /// directly. If another reader is still holding the same `Arc`
    /// (a persist snapshot, an api_stats snapshot, etc.), the entry
    /// is cloned in place first so the outside snapshot stays
    /// consistent.
    ///
    /// **Warning:** mutating `shard_hashes` through this handle
    /// bypasses `shard_refs` maintenance and leaves the index stale.
    /// The gateway's mutation callers (`repair_node`, async-encode
    /// failure path) either don't touch `shard_hashes` or perform a
    /// full [`Self::insert`] afterwards; if a new caller needs to
    /// mutate `shard_hashes` in place, `remove` + `insert` restores
    /// the invariant.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Manifest> {
        self.entries.get_mut(name).map(Arc::make_mut)
    }

    pub fn remove(&mut self, name: &str) -> Option<Manifest> {
        let arc = self.entries.remove(name)?;
        self.deindex(name, &arc);
        Some(Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
    }

    // ---- reverse-index maintenance -----------------------------------

    fn index(&mut self, name: &str, manifest: &Manifest) {
        for chan in &manifest.shard_hashes {
            for per_l in chan {
                for h in per_l {
                    self.shard_refs
                        .entry(*h)
                        .or_default()
                        .insert(name.to_string());
                }
            }
        }
    }

    fn deindex(&mut self, name: &str, manifest: &Manifest) {
        for chan in &manifest.shard_hashes {
            for per_l in chan {
                for h in per_l {
                    if let Some(refs) = self.shard_refs.get_mut(h) {
                        refs.remove(name);
                        if refs.is_empty() {
                            self.shard_refs.remove(h);
                        }
                    }
                }
            }
        }
    }

    /// Rebuild `shard_refs` from `entries`. Called after `decode`; also
    /// useful as an escape hatch for anyone who bypassed the maintained
    /// API (e.g. via `get_mut` mutating `shard_hashes`).
    pub fn rebuild_shard_index(&mut self) {
        self.shard_refs.clear();
        let entries: Vec<(String, Arc<Manifest>)> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        for (name, manifest) in entries {
            self.index(&name, &manifest);
        }
    }

    /// Return the subset of `candidate` hashes that no catalog entry
    /// (other than `exclude_name`, if any) still references. O(hashes)
    /// on the reverse index vs. the O(catalog × shards) full scan the
    /// gateway's `purge_orphans_of` used before v4.
    pub fn orphan_hashes<'a>(
        &self,
        candidate: impl IntoIterator<Item = &'a Hash>,
        exclude_name: Option<&str>,
    ) -> Vec<Hash> {
        candidate
            .into_iter()
            .filter(|h| match self.shard_refs.get(*h) {
                None => true,
                Some(refs) if refs.is_empty() => true,
                Some(refs) => match exclude_name {
                    Some(exc) => refs.iter().all(|n| n == exc),
                    None => false,
                },
            })
            .copied()
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Walk every non-directory entry and synthesize a `Directory` marker
    /// for every ancestor path that doesn't already have one. Used on boot
    /// to migrate pre-Stage-9 catalogs (which never wrote explicit
    /// directory entries) so the new tree-shaped UI can navigate them.
    /// Returns the number of new entries inserted.
    ///
    /// Directory `object_id`s come from the same domain-tagged SHA-256 the
    /// gateway uses in `mkdir`, so synthesized ids match gateway-minted
    /// ones for identical paths.
    pub fn synthesize_missing_directories(&mut self) -> usize {
        use holofs_core::hash::sha256;

        let mut wanted: Vec<String> = Vec::new();
        for (key, m) in &self.entries {
            if m.kind == ObjectKind::Directory {
                continue;
            }
            for anc in catalog_path::ancestors(key) {
                if !self.entries.contains_key(anc) {
                    wanted.push(anc.to_string());
                }
            }
        }
        wanted.sort();
        wanted.dedup();
        let n = wanted.len();
        for path in wanted {
            let mut buf = Vec::with_capacity(path.len() + 16);
            buf.extend_from_slice(b"holofs-dir-v1\0");
            buf.extend_from_slice(path.as_bytes());
            let h = sha256(&buf);
            let mut id = [0u8; 8];
            id.copy_from_slice(&h[..8]);
            let object_id = u64::from_be_bytes(id);
            // created_at = 0 — these placeholders are synthesized for
            // legacy catalogs and we have no honest timestamp for them.
            // Directory manifests carry no shards, so the reverse
            // index has nothing to add — safe to write `entries`
            // directly without going through `insert_arc`.
            self.entries
                .insert(path, Arc::new(Manifest::directory(object_id, 0)));
        }
        n
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for (name, manifest) in &self.entries {
            let nb = name.as_bytes();
            b.extend_from_slice(&(nb.len() as u16).to_be_bytes());
            b.extend_from_slice(nb);
            let mb = manifest.encode();
            b.extend_from_slice(&(mb.len() as u32).to_be_bytes());
            b.extend_from_slice(&mb);
        }
        b
    }

    /// Atomic write to a file: write to a per-call `.tmp.<pid>.<counter>`
    /// path, fsync, rename over the target. A crash in the middle
    /// leaves either the old valid catalog or the new valid one.
    ///
    /// Pre-N4 the tmp path was a single `<name>.tmp` shared across
    /// concurrent savers. Two racing calls would each write the same
    /// tmp then race their `rename`s; whichever lost the race saw
    /// `ENOENT` because the tmp had already moved. That was
    /// swallowed as an `eprintln!` from `persist_catalog`. Post-N4
    /// the same race would surface as a 500 on the losing PUT
    /// (`parallel_puts_to_distinct_names_all_succeed` reliably hit
    /// it in the e2e suite). Making each caller's tmp unique lets
    /// both saves succeed atomically — last-writer-wins by rename
    /// order, which is the same guarantee we advertised before.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> io::Result<()> {
        write_atomic(path, &self.encode())
    }
}

/// Write `bytes` atomically to `path` (tmp + rename). Public so
/// [`Gateway::persist_catalog`] can encode the catalog under a short
/// read-lock, drop the lock, and run the O(disk) tmp-write + rename
/// outside — before this the catalog RwLock was held across the
/// fs::write / fsync / rename, serialising every other reader for
/// milliseconds. Callers using [`Directory::save_atomic`] still get
/// the atomic semantics.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = match path.file_name() {
        Some(name) => {
            let mut fname = name.to_os_string();
            fname.push(format!(".tmp.{pid}.{n}"));
            path.with_file_name(fname)
        }
        None => path.with_extension(format!("tmp.{pid}.{n}")),
    };
    fs::write(&tmp, bytes)?;
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// no-op impl block that lets `save_atomic`'s trailing `}` close
/// [`Directory`]. Kept as a no-op so the `write_atomic` free
/// function above can live right next to `save_atomic` without
/// needing to jump around the file.
impl Directory {

    /// Load a catalog from a file. Missing file → empty catalog.
    pub fn load_or_empty(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        match fs::read(path) {
            Ok(bytes) => Self::decode(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < 12 || &buf[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a holofs catalog",
            ));
        }
        let mut pos = 8usize;
        let n = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let mut entries = BTreeMap::new();
        for _ in 0..n {
            if pos + 2 > buf.len() {
                return Err(eof());
            }
            let nlen = u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + nlen > buf.len() {
                return Err(eof());
            }
            let name = String::from_utf8(buf[pos..pos + nlen].to_vec())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("name: {e}")))?;
            pos += nlen;
            if pos + 4 > buf.len() {
                return Err(eof());
            }
            let mlen = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + mlen > buf.len() {
                return Err(eof());
            }
            let manifest = Manifest::decode(&buf[pos..pos + mlen])?;
            pos += mlen;
            entries.insert(name, Arc::new(manifest));
        }
        // shard_refs is derivable — build it once from the loaded
        // entries so `orphan_hashes` works from boot without a
        // wire-format bump. O(N × avg_shards) — a one-time boot cost.
        let mut dir = Directory { entries, shard_refs: HashMap::new() };
        dir.rebuild_shard_index();
        Ok(dir)
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "catalog truncated")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::Placement;

    fn fake_manifest(seed: u8) -> Manifest {
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
            kind: crate::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 1_700_000_000 + seed as u64,
            encoding: crate::manifest::ObjectEncoding::Rlnc,
            state: crate::manifest::ManifestState::Ready,
        }
    }

    #[test]
    fn directory_roundtrip_empty() {
        let d = Directory::new();
        let back = Directory::decode(&d.encode()).unwrap();
        assert_eq!(back, d);
        assert!(back.is_empty());
    }

    #[test]
    fn directory_roundtrip_with_entries() {
        let mut d = Directory::new();
        d.insert("photo_1.png".into(), fake_manifest(0));
        d.insert("photo_2.png".into(), fake_manifest(1));
        d.insert("doc/note.txt".into(), fake_manifest(2));
        let back = Directory::decode(&d.encode()).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.len(), 3);
        assert_eq!(
            back.names(),
            vec!["doc/note.txt", "photo_1.png", "photo_2.png"]
        );
    }

    #[test]
    fn directory_rejects_bad_magic() {
        let bad = vec![0u8; 32];
        assert!(Directory::decode(&bad).is_err());
    }

    #[test]
    fn directory_remove_works() {
        let mut d = Directory::new();
        d.insert("a".into(), fake_manifest(0));
        d.insert("b".into(), fake_manifest(1));
        assert!(d.remove("a").is_some());
        assert_eq!(d.len(), 1);
        assert!(d.get("a").is_none());
    }

    /// N4 regression guard: 8 threads calling `save_atomic` against the
    /// same path in parallel must all succeed. Pre-fix the shared
    /// `<name>.tmp` path caused racing renames to hit `ENOENT`, which
    /// pre-N4 was silently eaten and post-N4 surfaced as a 500 on the
    /// losing PUT (`parallel_puts_to_distinct_names_all_succeed` in
    /// the e2e suite).
    #[test]
    fn save_atomic_survives_concurrent_writers() {
        use std::sync::Arc;
        use std::thread;

        let dir = std::env::temp_dir().join(format!(
            "holofs-persist-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = Arc::new(dir.join("catalog.bin"));

        let mut handles = Vec::new();
        for i in 0..8 {
            let p = Arc::clone(&path);
            handles.push(thread::spawn(move || -> io::Result<()> {
                let mut d = Directory::new();
                d.insert(format!("obj-{i:02}.png"), fake_manifest(i));
                // Hammer save_atomic a handful of times so the tmp
                // filename races have plenty of chances to collide.
                for _ in 0..5 {
                    d.save_atomic(&*p)?;
                }
                Ok(())
            }));
        }
        for h in handles {
            let res = h.join().expect("thread panicked");
            assert!(res.is_ok(), "save_atomic Err under concurrency: {res:?}");
        }
        // The final catalog is whichever thread renamed last; it just
        // has to be *some* valid catalog.
        let back = Directory::load_or_empty(&*path).unwrap();
        assert!(!back.is_empty(), "final catalog was empty");
        // No leftover tmp files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".tmp.")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "concurrent save_atomic left tmp files behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_atomic_then_load_roundtrip() {
        let mut d = Directory::new();
        d.insert("a.png".into(), fake_manifest(1));
        d.insert("b.png".into(), fake_manifest(2));
        let dir = std::env::temp_dir().join(format!(
            "holofs-catalog-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("catalog.bin");
        d.save_atomic(&path).unwrap();
        let back = Directory::load_or_empty(&path).unwrap();
        assert_eq!(back, d);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_empty_returns_empty_when_missing() {
        let d = Directory::load_or_empty("/nonexistent/path/catalog.bin").unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn directory_get_returns_inserted() {
        let mut d = Directory::new();
        let m = fake_manifest(7);
        d.insert("photo.png".into(), m.clone());
        assert_eq!(d.get("photo.png"), Some(&m));
        assert_eq!(d.get("missing.png"), None);
    }

    #[test]
    fn synthesize_missing_directories_fills_implicit_prefixes() {
        let mut d = Directory::new();
        d.insert("photos/2026/a.png".into(), fake_manifest(1));
        d.insert("photos/2026/b.png".into(), fake_manifest(2));
        d.insert("docs/note.txt".into(), fake_manifest(3));
        d.insert("top.png".into(), fake_manifest(4));

        let added = d.synthesize_missing_directories();
        // Wanted dirs: "photos", "photos/2026", "docs". "top.png" has no
        // ancestors so contributes nothing.
        assert_eq!(added, 3);
        for path in &["photos", "photos/2026", "docs"] {
            let m = d.get(path).expect("directory marker present");
            assert_eq!(m.kind, ObjectKind::Directory);
            assert_ne!(m.object_id, 0);
        }
    }

    #[test]
    fn synthesize_missing_directories_is_idempotent() {
        let mut d = Directory::new();
        d.insert("a/b/c.txt".into(), fake_manifest(7));
        assert_eq!(d.synthesize_missing_directories(), 2);
        assert_eq!(d.synthesize_missing_directories(), 0);
        assert_eq!(d.len(), 3); // a, a/b, a/b/c.txt
    }

    #[test]
    fn synthesize_skips_when_directory_already_present() {
        let mut d = Directory::new();
        // Pre-existing Directory marker with a hand-picked object_id.
        d.insert("photos".into(), Manifest::directory(0xDEAD, 0));
        d.insert("photos/img.png".into(), fake_manifest(1));
        let added = d.synthesize_missing_directories();
        assert_eq!(added, 0);
        // The hand-picked id must survive untouched.
        assert_eq!(d.get("photos").unwrap().object_id, 0xDEAD);
    }

    // ---- v4 reverse-index tests --------------------------------------

    /// Build a manifest with known shard hashes so we can test the
    /// reverse index directly. Shape: 1 channel × 1 layer (opaque-
    /// style); `hashes` populates that slot.
    fn manifest_with_hashes(seed: u8, hashes: &[[u8; 32]]) -> Manifest {
        let mut m = fake_manifest(seed);
        m.channels = 1;
        m.nlayers = 1;
        m.n_per_layer = vec![hashes.len() as u32];
        m.sym_len = vec![64];
        m.layer_positions = vec![Vec::new()];
        m.shard_hashes = vec![vec![hashes.to_vec()]];
        m
    }

    fn h(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn shard_index_populated_on_insert() {
        let mut d = Directory::new();
        d.insert("a".into(), manifest_with_hashes(0, &[h(1), h(2), h(3)]));
        // Orphan check must return all three hashes now that no other
        // entry references them (from `a`'s point of view we exclude
        // "a" so its own refs don't count).
        let orphans = d.orphan_hashes(&[h(1), h(2), h(3)], Some("a"));
        assert_eq!(orphans.len(), 3);
    }

    #[test]
    fn shard_index_reflects_shared_hashes() {
        let mut d = Directory::new();
        d.insert("a".into(), manifest_with_hashes(0, &[h(1), h(2)]));
        d.insert("b".into(), manifest_with_hashes(1, &[h(2), h(3)]));
        // Removing `a`: h(1) becomes orphan, h(2) still held by `b`.
        let orphans = d.orphan_hashes(&[h(1), h(2)], None);
        // No exclude → h(2) has refs (b) → not orphan; h(1) has ref (a)
        // → not orphan. Both still referenced.
        assert_eq!(orphans, Vec::<[u8; 32]>::new());
        // Simulating DELETE-of-a: caller removes first, then asks.
        let removed = d.remove("a").unwrap();
        let owned: Vec<[u8; 32]> = removed.shard_hashes[0][0].clone();
        let orphans = d.orphan_hashes(owned.iter(), None);
        assert_eq!(orphans, vec![h(1)]);
    }

    #[test]
    fn shard_index_replace_swaps_refs() {
        let mut d = Directory::new();
        d.insert("a".into(), manifest_with_hashes(0, &[h(1), h(2)]));
        // Overwrite `a` with different shard hashes.
        d.insert("a".into(), manifest_with_hashes(1, &[h(3), h(4)]));
        // h(1)/h(2) now unreferenced.
        let orphans = d.orphan_hashes(&[h(1), h(2), h(3), h(4)], Some("a"));
        // Exclude "a" so `a`'s new refs are ignored — all four look
        // orphan from `a`'s frame of reference.
        assert_eq!(orphans.len(), 4);
        // Without excluding "a", h(3)/h(4) still ref'd, h(1)/h(2) not.
        let orphans = d.orphan_hashes(&[h(1), h(2), h(3), h(4)], None);
        assert_eq!(orphans, vec![h(1), h(2)]);
    }

    #[test]
    fn shard_index_remove_drops_refs() {
        let mut d = Directory::new();
        d.insert("a".into(), manifest_with_hashes(0, &[h(1)]));
        d.remove("a").unwrap();
        let orphans = d.orphan_hashes(&[h(1)], None);
        assert_eq!(orphans, vec![h(1)]);
    }

    #[test]
    fn shard_index_rebuild_matches_incremental() {
        // Build two identical catalogs — one via the maintained API,
        // one via `entries.insert` + `rebuild_shard_index`. Their
        // `orphan_hashes` outputs must agree for every candidate.
        let mut a = Directory::new();
        a.insert("x".into(), manifest_with_hashes(0, &[h(1), h(2)]));
        a.insert("y".into(), manifest_with_hashes(1, &[h(2), h(3)]));

        let mut b = Directory::new();
        b.entries.insert(
            "x".into(),
            Arc::new(manifest_with_hashes(0, &[h(1), h(2)])),
        );
        b.entries.insert(
            "y".into(),
            Arc::new(manifest_with_hashes(1, &[h(2), h(3)])),
        );
        b.rebuild_shard_index();

        for candidate in [h(1), h(2), h(3), h(9)] {
            assert_eq!(
                a.orphan_hashes([&candidate], None),
                b.orphan_hashes([&candidate], None),
                "candidate {candidate:?}"
            );
        }
    }

    #[test]
    fn shard_index_survives_decode_roundtrip() {
        // The wire format doesn't carry shard_refs; `decode` must
        // rebuild it. Verify by triggering a DELETE-style orphan
        // check on the decoded catalog.
        let mut d = Directory::new();
        d.insert("a".into(), manifest_with_hashes(0, &[h(1), h(2)]));
        d.insert("b".into(), manifest_with_hashes(1, &[h(2), h(3)]));

        let back = Directory::decode(&d.encode()).unwrap();
        let orphans = back.orphan_hashes(&[h(1), h(2), h(3)], Some("a"));
        // With "a" excluded: h(1) has only "a" (orphan), h(2) has "b"
        // (not orphan), h(3) has "b" (not orphan).
        assert_eq!(orphans, vec![h(1)]);
    }
}
