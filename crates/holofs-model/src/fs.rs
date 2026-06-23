//! Stage 4: file interface.
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

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::manifest::{Manifest, ObjectKind};
use crate::path as catalog_path;

const MAGIC: &[u8; 8] = b"HOLOFSD1";

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    pub entries: BTreeMap<String, Manifest>,
}

impl Directory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, manifest: Manifest) {
        self.entries.insert(name, manifest);
    }

    pub fn get(&self, name: &str) -> Option<&Manifest> {
        self.entries.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<Manifest> {
        self.entries.remove(name)
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
            self.entries
                .insert(path, Manifest::directory(object_id, 0));
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

    /// Atomic write to a file: write `.tmp`, fsync, rename. A crash in the
    /// middle leaves either the old valid catalog or the new valid one.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, self.encode())?;
        if let Ok(f) = fs::File::open(&tmp) {
            let _ = f.sync_all();
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

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
            entries.insert(name, manifest);
        }
        Ok(Directory { entries })
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
}
