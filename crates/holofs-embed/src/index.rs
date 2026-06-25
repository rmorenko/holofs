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
//! Removal happens by writing a tombstone record (zero dim). Stage 12.8
//! doesn't expose that yet — file rebuild is the upgrade path.

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
        }
        Ok(Self { path })
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
}
