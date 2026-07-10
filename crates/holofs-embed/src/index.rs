//! Append-only flat-file index for `(data_cid, layer_band) → embedding`.
//!
//! Format (little-endian unless noted):
//!
//! ```text
//! magic = "HOLOFEM1" (8 bytes)
//! repeat:
//!   data_cid   [u8; 32]
//!   band       u8        // LayerBand discriminant
//!   name_len   u16
//!   name_utf8  [u8; name_len]
//!   dim        u16
//!   vec        [f32; dim]   // L2-normalised
//! ```
//!
//! Append-only because catalog mutation is a much rarer event than
//! search; we re-scan the whole file on every query (50k entries × 512
//! f32 × cosine ≈ 5ms on CPU). When this stops scaling we'll layer an
//! HNSW index on top — but until then a flat scan is the smallest
//! moving part.
//!
//! Removal happens by writing a tombstone record (zero dim). //! doesn't expose that yet — file rebuild is the upgrade path.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::EmbedError;
use crate::{LayerBand, EMBED_DIM};

const MAGIC: &[u8; 8] = b"HOLOFEM1";

/// One record in the on-disk index.
#[derive(Debug, Clone)]
pub struct EmbedRecord {
    /// Identity from the manifest. Same value across re-uploads, so
    /// `(data_cid, band)` is the dedup key.
    pub data_cid: [u8; 32],
    /// Which layer band this embedding was computed against.
    pub band: LayerBand,
    /// Catalog name when the record was written. Stays for human
    /// readability — search results display the *current* catalog name
    /// looked up by data_cid, so renames after-the-fact don't break.
    pub name: String,
    /// `EMBED_DIM` floats, L2-normalised. Stored at full precision —
    /// 512 × 4 = 2 KiB per record. 50k records ≈ 100 MiB. Fine.
    pub vec: Vec<f32>,
}

/// Wrapper around the append-only flat file. Caller owns the path; the
/// gateway places it at `<storage_root>/embeddings.bin`.
pub struct Index {
    path: PathBuf,
}

impl Index {
    /// Open (or create + write magic header for) an index file at
    /// `path`. The file becomes valid the moment the magic is written
    /// so partial writes during the first PUT are recoverable.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, EmbedError> {
        let path = path.into();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = File::create(&path)?;
            f.write_all(MAGIC)?;
            f.sync_all()?;
        } else {
            // Verify the magic up front so we fail loudly instead of
            // appending records to a wrong-format file.
            let mut f = File::open(&path)?;
            let mut buf = [0u8; 8];
            f.read_exact(&mut buf).map_err(|_| {
                EmbedError::Corrupt("file shorter than magic header".into())
            })?;
            if &buf != MAGIC {
                return Err(EmbedError::Corrupt(format!(
                    "bad magic: expected {MAGIC:?}, got {buf:?}"
                )));
            }
            drop(f);
            // B13: truncate any partial record trailing the last
            // clean one. `IndexIter` already tolerates a torn tail
            // on read (it stops iteration on short reads), but
            // `append` writes at the current file end — which
            // includes those corrupt bytes. The next reader then
            // sees them anew each time. Rewinding to the last
            // clean boundary means one bad byte stays quarantined
            // to at most one restart.
            Self::truncate_torn_tail(&path)?;
        }
        Ok(Self { path })
    }

    /// Scan records from the start; find the byte offset of the last
    /// successfully-parsed record's end, and truncate the file to
    /// that offset. Called from [`Self::open`] so subsequent
    /// [`Self::append`] calls never write on top of stale garbage.
    fn truncate_torn_tail(path: &Path) -> Result<(), EmbedError> {
        let f = File::open(path)?;
        let file_len = f.metadata()?.len();
        let mut it = IndexIter {
            r: BufReader::new(f),
        };
        // Skip magic.
        let mut magic = [0u8; 8];
        it.r.read_exact(&mut magic)?;
        let mut last_clean: u64 = 8;
        let display = path.display().to_string();
        loop {
            let pos = it.r.stream_position()?;
            match it.next() {
                Some(Ok(_)) => {
                    last_clean = it.r.stream_position()?;
                }
                Some(Err(e)) => {
                    eprintln!(
                        "Index::open: {display}: dropping corrupt tail starting at offset {pos}: {e}"
                    );
                    break;
                }
                None => break,
            }
        }
        drop(it);
        if last_clean < file_len {
            let f = OpenOptions::new().write(true).open(path)?;
            f.set_len(last_clean)?;
            f.sync_all()?;
        }
        Ok(())
    }

    /// Path the index lives at — useful for logging / health output.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. Atomic per-record on append-mode writes
    /// (single OS write call); a torn write at the end of the file
    /// gets cleaned up by [`Self::iter`]'s end-of-file handling.
    pub fn append(&self, rec: &EmbedRecord) -> Result<(), EmbedError> {
        if rec.vec.len() != EMBED_DIM {
            return Err(EmbedError::BadInput(format!(
                "vec dim {} != EMBED_DIM {}",
                rec.vec.len(),
                EMBED_DIM
            )));
        }
        let f = OpenOptions::new().append(true).open(&self.path)?;
        let mut w = BufWriter::new(f);
        let name_bytes = rec.name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(EmbedError::BadInput("name too long".into()));
        }
        w.write_all(&rec.data_cid)?;
        w.write_all(&[rec.band as u8])?;
        w.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        w.write_all(name_bytes)?;
        w.write_all(&(rec.vec.len() as u16).to_le_bytes())?;
        for v in &rec.vec {
            w.write_all(&v.to_le_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Streaming iterator over the file. Skips a torn trailing record
    /// (treats it as "not present"); the next append will overwrite it.
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<EmbedRecord, EmbedError>>, EmbedError> {
        let f = File::open(&self.path)?;
        let mut r = BufReader::new(f);
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr)?;
        if &hdr != MAGIC {
            return Err(EmbedError::Corrupt("bad magic on iter".into()));
        }
        Ok(IndexIter { r })
    }

    /// Whether a `(data_cid, band)` pair is already indexed. Linear
    /// scan; called only on PUT, so it's not on the hot path.
    pub fn has(&self, data_cid: &[u8; 32], band: LayerBand) -> Result<bool, EmbedError> {
        for rec in self.iter()? {
            let rec = rec?;
            if &rec.data_cid == data_cid && rec.band == band {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Count of records on disk. Useful for `/health` /
    /// `holofs embed-all` progress reporting.
    pub fn len(&self) -> Result<usize, EmbedError> {
        let mut n = 0;
        for r in self.iter()? {
            r?;
            n += 1;
        }
        Ok(n)
    }

    /// rewrite the index keeping only records for which
    /// `keep(data_cid)` returns `true`. Atomic-on-success: writes to
    /// `<path>.tmp` first, then renames over the original. Returns
    /// `(kept, dropped)` counts. Tombstones (`vec.is_empty()`) are
    /// always dropped — they're already-deleted records.
    pub fn rewrite_keep<F>(&self, mut keep: F) -> Result<(usize, usize), EmbedError>
    where
        F: FnMut(&[u8; 32]) -> bool,
    {
        let tmp = self.path.with_extension("bin.tmp");
        // Start a fresh file (magic header) at the temp path.
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_f = std::fs::File::create(&tmp)?;
        tmp_f.write_all(MAGIC)?;
        tmp_f.sync_all()?;
        drop(tmp_f);

        let mut kept = 0usize;
        let mut dropped = 0usize;
        {
            // Reopen the tmp file in append mode through Index for
            // the append API uniformity.
            let tmp_idx = Self {
                path: tmp.clone(),
            };
            for rec in self.iter()? {
                let rec = rec?;
                if rec.vec.is_empty() {
                    dropped += 1;
                    continue;
                }
                if keep(&rec.data_cid) {
                    tmp_idx.append(&rec)?;
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok((kept, dropped))
    }
}

struct IndexIter<R: Read + Seek> {
    r: R,
}

impl<R: Read + Seek> Iterator for IndexIter<R> {
    type Item = Result<EmbedRecord, EmbedError>;

    fn next(&mut self) -> Option<Self::Item> {
        let pos = self.r.stream_position().ok()?;
        let mut data_cid = [0u8; 32];
        match self.r.read_exact(&mut data_cid) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
            Err(e) => return Some(Err(e.into())),
        }
        let mut band_byte = [0u8; 1];
        if self.r.read_exact(&mut band_byte).is_err() {
            // Torn record at EOF — pretend it never existed.
            let _ = self.r.seek(SeekFrom::Start(pos));
            return None;
        }
        let Some(band) = LayerBand::from_u8(band_byte[0]) else {
            return Some(Err(EmbedError::Corrupt(format!(
                "unknown band discriminant {}",
                band_byte[0]
            ))));
        };
        let mut nl = [0u8; 2];
        if self.r.read_exact(&mut nl).is_err() {
            return None;
        }
        let name_len = u16::from_le_bytes(nl) as usize;
        let mut name_buf = vec![0u8; name_len];
        if self.r.read_exact(&mut name_buf).is_err() {
            return None;
        }
        let name = String::from_utf8(name_buf)
            .map_err(|e| EmbedError::Corrupt(format!("bad name utf-8: {e}")));
        let name = match name {
            Ok(n) => n,
            Err(e) => return Some(Err(e)),
        };
        let mut dl = [0u8; 2];
        if self.r.read_exact(&mut dl).is_err() {
            return None;
        }
        let dim = u16::from_le_bytes(dl) as usize;
        if dim == 0 {
            // Tombstone — caller skips by checking dim. We surface it.
            return Some(Ok(EmbedRecord {
                data_cid,
                band,
                name,
                vec: Vec::new(),
            }));
        }
        if dim != EMBED_DIM {
            return Some(Err(EmbedError::Corrupt(format!(
                "dim {dim} != EMBED_DIM {}",
                EMBED_DIM
            ))));
        }
        let mut vec = vec![0f32; dim];
        for slot in vec.iter_mut() {
            let mut b = [0u8; 4];
            if self.r.read_exact(&mut b).is_err() {
                return None;
            }
            *slot = f32::from_le_bytes(b);
        }
        Some(Ok(EmbedRecord {
            data_cid,
            band,
            name,
            vec,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_append_iter() {
        let tmp = std::env::temp_dir().join("holofs-embed-test.bin");
        let _ = std::fs::remove_file(&tmp);
        let idx = Index::open(&tmp).unwrap();
        let rec = EmbedRecord {
            data_cid: [7u8; 32],
            band: LayerBand::Coarse,
            name: "photos/cat.png".into(),
            vec: vec![0.0; EMBED_DIM],
        };
        idx.append(&rec).unwrap();
        let read: Vec<_> = idx.iter().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].data_cid, rec.data_cid);
        assert_eq!(read[0].name, rec.name);
        assert_eq!(read[0].vec.len(), EMBED_DIM);
        assert!(idx.has(&rec.data_cid, LayerBand::Coarse).unwrap());
        assert!(!idx.has(&rec.data_cid, LayerBand::Full).unwrap());
        let _ = std::fs::remove_file(&tmp);
    }

    /// Helper: temp path that the test's Drop tidies up.
    struct TempPath(std::path::PathBuf);
    impl TempPath {
        fn new(stem: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "holofs-embed-{stem}-{}.bin",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&p);
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn rec(seed: u8, band: LayerBand, name: &str) -> EmbedRecord {
        EmbedRecord {
            data_cid: [seed; 32],
            band,
            name: name.to_string(),
            vec: vec![(seed as f32) / 100.0; EMBED_DIM],
        }
    }

    #[test]
    fn multiple_appends_iterate_in_insertion_order() {
        let t = TempPath::new("multi");
        let idx = Index::open(t.path()).unwrap();
        idx.append(&rec(1, LayerBand::Coarse, "a.png")).unwrap();
        idx.append(&rec(2, LayerBand::Mid, "b.png")).unwrap();
        idx.append(&rec(3, LayerBand::Full, "c.png")).unwrap();
        let names: Vec<String> = idx
            .iter()
            .unwrap()
            .map(|r| r.unwrap().name)
            .collect();
        assert_eq!(names, vec!["a.png", "b.png", "c.png"]);
        assert_eq!(idx.len().unwrap(), 3);
    }

    #[test]
    fn has_distinguishes_band_per_cid() {
        let t = TempPath::new("hasband");
        let idx = Index::open(t.path()).unwrap();
        idx.append(&rec(1, LayerBand::Coarse, "x.png")).unwrap();
        idx.append(&rec(1, LayerBand::Mid, "x.png")).unwrap();
        let cid = [1u8; 32];
        assert!(idx.has(&cid, LayerBand::Coarse).unwrap());
        assert!(idx.has(&cid, LayerBand::Mid).unwrap());
        assert!(!idx.has(&cid, LayerBand::Full).unwrap());
        // Different CID — none of the bands match.
        let other = [9u8; 32];
        assert!(!idx.has(&other, LayerBand::Coarse).unwrap());
    }

    #[test]
    fn reopening_existing_index_preserves_records() {
        let t = TempPath::new("reopen");
        {
            let idx = Index::open(t.path()).unwrap();
            idx.append(&rec(5, LayerBand::Coarse, "first.png")).unwrap();
        }
        // Drop the handle and re-open from the same path.
        let idx = Index::open(t.path()).unwrap();
        assert_eq!(idx.len().unwrap(), 1);
        let collected: Vec<_> = idx.iter().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(collected[0].name, "first.png");
    }

    #[test]
    fn opening_file_with_wrong_magic_returns_corrupt() {
        let t = TempPath::new("badmagic");
        std::fs::write(t.path(), b"NOTHOLO\0lots of junk follows").unwrap();
        let err = Index::open(t.path()).err().expect("expected Corrupt error");
        assert!(matches!(err, EmbedError::Corrupt(_)));
    }

    #[test]
    fn opening_file_shorter_than_magic_returns_corrupt() {
        let t = TempPath::new("short");
        std::fs::write(t.path(), b"HO").unwrap();
        let err = Index::open(t.path()).err().expect("expected Corrupt error");
        assert!(matches!(err, EmbedError::Corrupt(_)));
    }

    #[test]
    fn rewrite_keep_drops_filtered_records_and_tombstones() {
        let t = TempPath::new("rewrite");
        let idx = Index::open(t.path()).unwrap();
        idx.append(&rec(1, LayerBand::Coarse, "keep.png")).unwrap();
        idx.append(&rec(2, LayerBand::Coarse, "drop.png")).unwrap();
        // Hand-write a tombstone record (dim=0). The public `append`
        // refuses dim≠EMBED_DIM, so tombstones get into the file via
        // the gateway's batch path (or this raw write). rewrite_keep
        // must always drop them regardless of the keep predicate.
        {
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(t.path()).unwrap();
            let cid = [9u8; 32];
            let band = LayerBand::Coarse as u8;
            let name = b"tomb.png";
            f.write_all(&cid).unwrap();
            f.write_all(&[band]).unwrap();
            f.write_all(&(name.len() as u16).to_le_bytes()).unwrap();
            f.write_all(name).unwrap();
            f.write_all(&0u16.to_le_bytes()).unwrap(); // dim = 0 → tombstone
        }
        let (kept, dropped) = idx.rewrite_keep(|cid| cid[0] == 1).unwrap();
        assert_eq!(kept, 1);
        assert!(dropped >= 2, "expected to drop drop.png + tomb.png, got {dropped}");
        let names: Vec<String> = idx
            .iter()
            .unwrap()
            .map(|r| r.unwrap().name)
            .collect();
        assert_eq!(names, vec!["keep.png"]);
    }

    #[test]
    fn rewrite_keep_with_all_true_predicate_keeps_everything() {
        let t = TempPath::new("rewriteall");
        let idx = Index::open(t.path()).unwrap();
        idx.append(&rec(1, LayerBand::Coarse, "a.png")).unwrap();
        idx.append(&rec(2, LayerBand::Mid, "b.png")).unwrap();
        let (kept, dropped) = idx.rewrite_keep(|_| true).unwrap();
        assert_eq!(kept, 2);
        assert_eq!(dropped, 0);
        assert_eq!(idx.len().unwrap(), 2);
    }

    #[test]
    fn path_accessor_returns_the_open_path() {
        let t = TempPath::new("pathaccess");
        let idx = Index::open(t.path()).unwrap();
        assert_eq!(idx.path(), t.path());
    }

    /// B13: after `open` sees a corrupt record it truncates the tail
    /// so the index is left in a self-consistent state (subsequent
    /// `append` writes on clean bytes). Here the corrupt record is
    /// the whole file → index becomes empty.
    #[test]
    fn open_truncates_corrupt_band_byte() {
        let t = TempPath::new("corruptband");
        let mut buf = Vec::new();
        buf.extend_from_slice(b"HOLOFEM1");
        buf.extend_from_slice(&[0u8; 32]); // data_cid
        buf.push(0xFF); // bad band
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(b"abc");
        buf.extend_from_slice(&(EMBED_DIM as u16).to_le_bytes());
        for _ in 0..EMBED_DIM {
            buf.extend_from_slice(&0f32.to_le_bytes());
        }
        std::fs::write(t.path(), &buf).unwrap();
        let idx = Index::open(t.path()).unwrap();
        // Corrupt record was truncated → empty index.
        assert_eq!(idx.len().unwrap(), 0);
    }

    /// B13: torn tail *after* one good record — the good record must
    /// survive, the trailing garbage must be gone. Before B13 the
    /// next append would land on top of the garbage and every
    /// subsequent iter would either surface the garbage as Err or
    /// silently skip beyond it, producing a mixed-in ghost record.
    #[test]
    fn open_preserves_good_record_and_drops_torn_tail() {
        let t = TempPath::new("goodplustorn");
        let idx = Index::open(t.path()).unwrap();
        let good = EmbedRecord {
            data_cid: [7u8; 32],
            band: LayerBand::Coarse,
            name: "keeper".into(),
            vec: vec![0.5; EMBED_DIM],
        };
        idx.append(&good).unwrap();
        drop(idx);
        // Simulate a torn append: 33 bytes of half-record garbage.
        {
            let mut f = OpenOptions::new().append(true).open(t.path()).unwrap();
            f.write_all(&[0xEEu8; 33]).unwrap();
        }
        let file_len_before = std::fs::metadata(t.path()).unwrap().len();
        let idx = Index::open(t.path()).unwrap();
        let file_len_after = std::fs::metadata(t.path()).unwrap().len();
        assert!(
            file_len_after < file_len_before,
            "expected torn tail dropped: before={file_len_before}, after={file_len_after}"
        );
        assert_eq!(idx.len().unwrap(), 1);
        let recs: Vec<_> = idx.iter().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "keeper");
    }
}
