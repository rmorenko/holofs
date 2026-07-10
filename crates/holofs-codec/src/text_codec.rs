//! codec for text files.
//!
//! The text is split into exactly K chunks along **UTF-8 character boundaries**
//! (multibyte characters are not cut). Chunks are zero-padded to a common
//! `sym_len`; the original chunk lengths are stored in `manifest.chunk_lens`.
//!
//! Chunks are then encoded via [`holofs_core::rlnc::encode_layer`] — the first
//! K shards are systematic (literally contain the i-th chunk), and shards
//! beyond K are normal RLNC combinations for redundancy.
//!
//! Decoding uses [`holofs_core::rlnc::decode_layer_with_holes`] — when too few
//! shards are available, the chunks that *could* be recovered are returned,
//! and the rest are marked as "holes". This yields **partial text recovery**:
//! "the rest is readable; positions X..Y contain a `[missing chunk N]` marker".

use holofs_core::K;

const MISSING_MARKER: &[u8] = b"\n[--missing chunk #";
const MISSING_MARKER_END: &[u8] = b" not recoverable--]\n";

/// Result of splitting text into K chunks.
pub struct TextSplit {
    /// K chunks, zero-padded to `sym_len`.
    pub padded: Vec<u8>,
    /// Symbol length (common padded chunk length).
    pub sym_len: usize,
    /// Original chunk lengths (without padding) — stored in `manifest.chunk_lens`.
    pub chunk_lens: Vec<u32>,
}

/// Split text into K chunks along UTF-8 character boundaries. Strategy: divide
/// the byte length evenly into K, then shift each boundary forward to the
/// nearest UTF-8 character boundary. Finally zero-pad to a common `sym_len`.
pub fn split_text_into_k_chunks(text: &str) -> TextSplit {
    let bytes = text.as_bytes();
    let n = bytes.len();

    // K split points: indices 1..K-1 (0 and n are the outer boundaries) → K chunks.
    let mut splits: Vec<usize> = (0..=K).map(|i| (i * n) / K).collect();
    // Shift each split forward to a UTF-8 char boundary.
    for s in splits.iter_mut().skip(1).take(K - 1) {
        while *s < n && !text.is_char_boundary(*s) {
            *s += 1;
        }
    }
    splits[0] = 0;
    splits[K] = n;

    let chunks: Vec<&[u8]> = (0..K).map(|i| &bytes[splits[i]..splits[i + 1]]).collect();
    let max_len = chunks.iter().map(|c| c.len()).max().unwrap_or(0).max(1);
    let chunk_lens: Vec<u32> = chunks.iter().map(|c| c.len() as u32).collect();

    let mut padded = vec![0u8; K * max_len];
    for (i, c) in chunks.iter().enumerate() {
        padded[i * max_len..i * max_len + c.len()].copy_from_slice(c);
    }
    TextSplit {
        padded,
        sym_len: max_len,
        chunk_lens,
    }
}

/// Reassemble text from K (possibly incomplete) chunks.
/// `Some(chunk)` — chunk recovered, trimmed to `chunk_lens[i]`.
/// `None` — chunk lost; a `[missing chunk #i]` marker is inserted.
pub fn assemble_text_with_holes(
    chunks: &[Option<Vec<u8>>],
    chunk_lens: &[u32],
) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut holes = 0usize;
    for (i, c) in chunks.iter().enumerate() {
        match c {
            Some(bytes) => {
                let real_len = chunk_lens.get(i).copied().unwrap_or(bytes.len() as u32) as usize;
                out.extend_from_slice(&bytes[..real_len.min(bytes.len())]);
            }
            None => {
                holes += 1;
                out.extend_from_slice(MISSING_MARKER);
                out.extend_from_slice(i.to_string().as_bytes());
                out.extend_from_slice(MISSING_MARKER_END);
            }
        }
    }
    (out, holes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_roundtrip_ascii() {
        let text = "Hello world, this is a fairly long ASCII string that we split into chunks.";
        let split = split_text_into_k_chunks(text);
        assert_eq!(split.chunk_lens.len(), K);
        assert_eq!(split.padded.len(), K * split.sym_len);
        // Sum of chunk lengths must equal the source length.
        let total: u32 = split.chunk_lens.iter().sum();
        assert_eq!(total as usize, text.len());

        // Recover from all "intact" chunks.
        let chunks: Vec<Option<Vec<u8>>> = (0..K)
            .map(|i| Some(split.padded[i * split.sym_len..(i + 1) * split.sym_len].to_vec()))
            .collect();
        let (back, holes) = assemble_text_with_holes(&chunks, &split.chunk_lens);
        assert_eq!(holes, 0);
        assert_eq!(back, text.as_bytes());
    }

    #[test]
    fn split_respects_utf8_boundaries() {
        // Text with multibyte UTF-8 characters. A naive byte-boundary split
        // would yield invalid UTF-8 in chunks. The Cyrillic source string is
        // intentional test data — do not translate.
        let text = "Привет мир, это длинная UTF-8 строка с кириллицей и эмодзи 🚀🎉 для проверки.";
        let split = split_text_into_k_chunks(text);
        // Each individual chunk (trimmed to chunk_len) must be valid UTF-8.
        for i in 0..K {
            let len = split.chunk_lens[i] as usize;
            let chunk = &split.padded[i * split.sym_len..i * split.sym_len + len];
            assert!(
                std::str::from_utf8(chunk).is_ok(),
                "chunk {i} is not valid UTF-8: {:?}",
                chunk
            );
        }
        // Concatenation must yield the source.
        let chunks: Vec<Option<Vec<u8>>> = (0..K)
            .map(|i| Some(split.padded[i * split.sym_len..(i + 1) * split.sym_len].to_vec()))
            .collect();
        let (back, _) = assemble_text_with_holes(&chunks, &split.chunk_lens);
        assert_eq!(back, text.as_bytes());
    }

    #[test]
    fn assemble_inserts_marker_for_missing() {
        // 3 live chunks + 1 hole — result contains the marker.
        let chunks = vec![
            Some(b"AAA".to_vec()),
            None,
            Some(b"CCC".to_vec()),
            Some(b"DDD".to_vec()),
        ];
        let lens = vec![3, 3, 3, 3];
        let (out, holes) = assemble_text_with_holes(&chunks, &lens);
        assert_eq!(holes, 1);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("AAA"));
        assert!(s.contains("CCC"));
        assert!(s.contains("DDD"));
        assert!(s.contains("missing chunk #1"));
    }

    #[test]
    fn short_text_still_makes_k_chunks() {
        // Text shorter than K bytes — some chunks are empty, but the path still works.
        let text = "hello";
        let split = split_text_into_k_chunks(text);
        assert_eq!(split.chunk_lens.len(), K);
        let total: u32 = split.chunk_lens.iter().sum();
        assert_eq!(total as usize, text.len());
    }
}

// === bottom-K MinHash for text similarity (moved here from
// holofs-analytics::shingle so `holofs-client::put_text_object` no
// longer needs to depend on `holofs-analytics` — see S4-2 of the
// review; that dependency was the only backward edge in an
// otherwise clean crate DAG). Analytics still re-exports these so
// existing gateway callers keep working with unchanged imports.

/// Shingle (n-gram) size in bytes. 5 bytes is the standard choice for text.
pub const MINHASH_SHINGLE_SIZE: usize = 5;

/// MinHash fingerprint size in u32 values. 64 → ~12 % Jaccard estimation error.
pub const MINHASH_K: usize = 64;

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h = 0x811c9dc5u32;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Compute a text's MinHash fingerprint: bottom-K minimum hash values of
/// shingles. If the text is shorter than `MINHASH_SHINGLE_SIZE`, treat the
/// whole text as a single shingle. Result length =
/// `min(MINHASH_K, unique_shingle_count)`.
pub fn compute_minhash(text: &str) -> Vec<u32> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut hashes: Vec<u32> = if bytes.len() < MINHASH_SHINGLE_SIZE {
        vec![fnv1a(bytes)]
    } else {
        let mut acc = Vec::with_capacity(bytes.len() - MINHASH_SHINGLE_SIZE + 1);
        for i in 0..=bytes.len() - MINHASH_SHINGLE_SIZE {
            acc.push(fnv1a(&bytes[i..i + MINHASH_SHINGLE_SIZE]));
        }
        acc
    };
    hashes.sort_unstable();
    hashes.dedup();
    hashes.truncate(MINHASH_K);
    hashes
}
