//! bottom-k MinHash for fuzzy similar-text search.
//!
//! Take text → 5-grams (byte-wise) → FNV-1a hash → bottom-K minimum values.
//! The result is a fixed-size fingerprint (`MINHASH_K = 64` u32 = 256 bytes)
//! that can be compared via Jaccard similarity.
//!
//! ## Properties
//!
//! - **MinHash approximates Jaccard**: for two bottom-K MinHash sets A and B,
//!   `|intersection(A, B)| / K ≈ Jaccard(set_A, set_B)`.
//! - Identical texts → fingerprints match byte-for-byte.
//! - Small edits (typos, paragraph additions, reorderings) → Jaccard usually
//!   stays ≥ 0.85.
//! - Completely different texts → Jaccard close to 0.
//!
//! Useful for:
//! - **Plagiarism detection** — parts of one document appearing in another.
//! - **Dedup of edited texts** — plain SHA-256 misses these, MinHash finds them.
//! - **Find drafts of the same document** — multiple versions of an article.
//!
//! ## Not the same as perceptual fingerprint (L0)
//!
//! A perceptual fingerprint catches "structurally similar" data (whole
//! documents/images/audio with the same overall content). MinHash catches
//! **partial overlaps** — even if 60% of text A is contained in text B,
//! Jaccard reports it.

/// Shingle (n-gram) size in bytes. 5 bytes is the standard choice for text.
///
/// Re-exports the canonical constant from `holofs-codec::text_codec`
/// so gateway/similarity callers can continue to import from
/// `holofs_analytics::shingle::SHINGLE_SIZE` — the actual definition
/// lives in codec now (see S4-2 of the review: killing the
/// `holofs-client → holofs-analytics` back-edge).
pub const SHINGLE_SIZE: usize = holofs_codec::text_codec::MINHASH_SHINGLE_SIZE;

/// MinHash fingerprint size in u32 values. 64 → ~12% Jaccard estimation error.
pub const MINHASH_K: usize = holofs_codec::text_codec::MINHASH_K;

/// Compute a text's MinHash fingerprint. Thin re-export of
/// [`holofs_codec::text_codec::compute_minhash`] — kept here so
/// existing callers (`gateway/fingerprint`, `gateway/similarity`)
/// don't need import updates.
pub fn compute_minhash(text: &str) -> Vec<u32> {
    holofs_codec::text_codec::compute_minhash(text)
}

/// Estimate Jaccard similarity via bottom-K MinHash.
///
/// Algorithm: for two **sorted** bottom-K sets A and B take the union, keep
/// the top-K minimum entries, count how many of them appear in **both**
/// sets. This is an unbiased Jaccard estimator.
///
/// Returns a value in [0.0, 1.0]. Identical → 1.0, disjoint → 0.0.
pub fn jaccard_similarity(a: &[u32], b: &[u32]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // Merge two sorted lists with dedup; pick the bottom-K.
    let k = MINHASH_K.min(a.len()).min(b.len());
    let mut union_topk: Vec<u32> = Vec::with_capacity(2 * k);
    let mut ai = 0usize;
    let mut bi = 0usize;
    while union_topk.len() < k && (ai < a.len() || bi < b.len()) {
        let next = match (a.get(ai), b.get(bi)) {
            (Some(&x), Some(&y)) if x < y => {
                ai += 1;
                x
            }
            (Some(&x), Some(&y)) if x > y => {
                bi += 1;
                y
            }
            (Some(&x), Some(_)) => {
                // x == y
                ai += 1;
                bi += 1;
                x
            }
            (Some(&x), None) => {
                ai += 1;
                x
            }
            (None, Some(&y)) => {
                bi += 1;
                y
            }
            (None, None) => break,
        };
        if union_topk.last() != Some(&next) {
            union_topk.push(next);
        }
    }
    // Count how many of union_topk appear in both sets.
    let in_both = union_topk
        .iter()
        .filter(|h| a.binary_search(h).is_ok() && b.binary_search(h).is_ok())
        .count();
    in_both as f32 / union_topk.len() as f32
}

/// Human-readable hex preview of a MinHash.
pub fn minhash_hex_preview(mh: &[u32]) -> String {
    let mut out = String::with_capacity(mh.len() * 9);
    for (i, &h) in mh.iter().enumerate().take(8) {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{h:08x}"));
    }
    if mh.len() > 8 {
        out.push_str(" …");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_give_jaccard_1() {
        let text = "The quick brown fox jumps over the lazy dog";
        let a = compute_minhash(text);
        let b = compute_minhash(text);
        assert_eq!(a, b);
        assert_eq!(jaccard_similarity(&a, &b), 1.0);
    }

    #[test]
    fn completely_different_texts_give_low_jaccard() {
        let a = compute_minhash("foo bar baz qux frob nicate xyzzy");
        let b = compute_minhash("aaaaaa bbbbbb cccccc dddddd eeeeee");
        let j = jaccard_similarity(&a, &b);
        assert!(j < 0.05, "expected near 0, got {j}");
    }

    #[test]
    fn small_edit_preserves_high_similarity() {
        let original = "The quick brown fox jumps over the lazy dog. \
                        It runs through the forest and finds a stream. \
                        The stream is cold and clear. Fish swim in it.";
        let edited = "The quick brown fox jumps over the lazy dog. \
                      It runs through the forest and finds a river. \
                      The river is cold and clear. Fish swim in it.";
        let a = compute_minhash(original);
        let b = compute_minhash(edited);
        let j = jaccard_similarity(&a, &b);
        assert!(j > 0.75, "small edit Jaccard {j}, expected > 0.75");
    }

    #[test]
    fn partial_inclusion_shows_meaningful_jaccard() {
        let full = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                    Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";
        let half = "Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";
        let a = compute_minhash(full);
        let b = compute_minhash(half);
        let j = jaccard_similarity(&a, &b);
        // ~50% shared shingles → Jaccard around 0.3-0.5.
        assert!(j > 0.2 && j < 0.7, "partial inclusion Jaccard {j}");
    }

    #[test]
    fn empty_text_gives_empty_fingerprint() {
        let a = compute_minhash("");
        assert!(a.is_empty());
        let j = jaccard_similarity(&a, &compute_minhash("hello"));
        assert_eq!(j, 0.0);
    }

    #[test]
    fn jaccard_is_symmetric() {
        let a = compute_minhash("aaa bbb ccc ddd eee");
        let b = compute_minhash("bbb ccc ddd eee fff");
        let j1 = jaccard_similarity(&a, &b);
        let j2 = jaccard_similarity(&b, &a);
        assert_eq!(j1, j2);
    }

    #[test]
    fn minhash_size_bounded_by_k() {
        // A rich text must produce exactly MINHASH_K values (after dedup).
        let long = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                    eiusmod tempor incididunt ut labore et dolore magna aliqua \
                    ut enim ad minim veniam quis nostrud exercitation ullamco \
                    laboris nisi ut aliquip ex ea commodo consequat duis aute \
                    irure dolor in reprehenderit in voluptate velit esse cillum";
        let mh = compute_minhash(long);
        assert_eq!(
            mh.len(),
            MINHASH_K,
            "a long rich text must produce exactly K"
        );
    }

    #[test]
    fn periodic_text_dedups_to_few_shingles() {
        // Periodic text has few unique shingles — that's correct.
        let periodic = "abcdefghij".repeat(1000);
        let mh = compute_minhash(&periodic);
        assert_eq!(mh.len(), 10, "10 unique 5-shingles in abcdefghij");
    }
}
