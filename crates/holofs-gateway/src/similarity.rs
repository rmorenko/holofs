//! Similarity types + scope helpers.
//!
//! The `similar_to` method itself (which USES these types) is still
//! in `http_gateway.rs` because it shares its `impl Gateway` block
//! with `chunk_diff` and shard-cache helpers. This module carries
//! the public shape only:
//!
//! - `SimilarScope` — folder / tree / all filter
//! - `SimilarityMethod` — Jaccard vs dHash
//! - `SimilarMatch` — one hit row
//! - `ShardOverlap` — cross-object shard-hash overlap row
//! - `SimilarReport` — the whole response
//! - `parent_dir` / `in_scope` — helpers used by both `similar_to`
//!   and the unit tests
//!

/// Scope filter for [`Gateway::similar_to`]. Constrains the candidate
/// pool relative to the target object's parent directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarScope {
    /// Whole catalog (legacy default).
    All,
    /// Only direct siblings — entries whose parent dir equals the
    /// target's parent dir.
    Folder,
    /// Subtree — entries whose path starts at the target's parent dir.
    /// At the catalog root this collapses to `All`.
    Tree,
}

impl SimilarScope {
    /// Parse a query-string value (`all` / `folder` / `tree`). Unknown
    /// values fall back to `All` so a hand-edited URL can't break the page.
    pub fn parse(s: &str) -> Self {
        match s {
            "folder" => Self::Folder,
            "tree" => Self::Tree,
            _ => Self::All,
        }
    }
}

/// Directory portion of a catalog name. `"a/b/c.png"` → `"a/b"`;
/// `"top.png"` → `""`.
pub(crate) fn parent_dir(name: &str) -> &str {
    name.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// Whether `candidate` is in the comparison pool for a target whose
/// parent directory is `target_parent`, under the given `scope`.
pub(crate) fn in_scope(target_parent: &str, candidate: &str, scope: SimilarScope) -> bool {
    match scope {
        SimilarScope::All => true,
        SimilarScope::Folder => parent_dir(candidate) == target_parent,
        SimilarScope::Tree => {
            if target_parent.is_empty() {
                // Target sits at the catalog root — tree scope spans the
                // whole catalog, indistinguishable from All.
                true
            } else {
                // Either inside target_parent directly or anywhere below it.
                candidate.starts_with(&format!("{target_parent}/"))
            }
        }
    }
}

/// Comparison method used in [`SimilarMatch::method`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarityMethod {
    /// MinHash Jaccard over 5-shingles (text).
    Jaccard,
    /// Hamming distance over the 45-bit dHash derived from the per-channel
    /// 48-byte perceptual fingerprint (image/audio).
    DHash,
}

/// One row of the top-similar table on `/similar/<name>`.
#[derive(Debug, Clone)]
pub struct SimilarMatch {
    /// Catalog name of the matched object.
    pub name: String,
    /// Percentage in `[0.0, 100.0]`.
    pub similarity_pct: f32,
    /// Comparison method that produced `similarity_pct`.
    pub method: SimilarityMethod,
}

/// One row of the shard-overlap table on `/similar/<name>`.
#[derive(Debug, Clone)]
pub struct ShardOverlap {
    /// Catalog name of the other object.
    pub name: String,
    /// Count of shard hashes shared with the target object.
    pub common: usize,
    /// `common / total_target * 100`.
    pub overlap_pct: f32,
    /// percentage of *low-layer* (structure / silhouette)
    /// shards of the target that this neighbour also carries. Computed
    /// over the bottom half of layers — for the typical `nlayers=8`
    /// image that's L0..=L3.
    pub low_layer_overlap_pct: f32,
    /// Percentage of *high-layer* (detail / texture) shards shared.
    /// Top half of layers.
    pub high_layer_overlap_pct: f32,
    /// `low - high` percentage points. Positive values flag "robust
    /// copies": files where the structure is preserved (low layers
    /// hash-identical) but detail differs — exactly what a watermark,
    /// recompression, or light retouch produces.
    pub robust_copy_score: f32,
}

/// Result of [`Gateway::similar_to`].
#[derive(Debug, Clone)]
pub struct SimilarReport {
    /// Target catalog name (the object the page is rendered for).
    pub name: String,
    /// Target object kind — drives the methodology blurb.
    pub kind: holofs_model::manifest::ObjectKind,
    /// Hex preview of the comparison fingerprint (MinHash for text, FP for media).
    pub fingerprint_hex: String,
    /// Length of the MinHash sketch (= [`holofs_analytics::shingle::MINHASH_K`]).
    pub minhash_k: usize,
    /// Total shard count of the target object (denominator for overlap %).
    pub total_shards: usize,
    /// Top-10 neighbours of the same kind, sorted by similarity desc.
    pub neighbors: Vec<SimilarMatch>,
    /// Cross-object shard-hash overlaps (any kind), sorted by `common` desc.
    pub overlaps: Vec<ShardOverlap>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_strips_last_segment() {
        assert_eq!(parent_dir("a/b/c.png"), "a/b");
        assert_eq!(parent_dir("top.png"), "");
        assert_eq!(parent_dir("only/one.png"), "only");
    }

    #[test]
    fn scope_all_keeps_everything() {
        assert!(in_scope("photos/2024", "anything/else.png", SimilarScope::All));
        assert!(in_scope("", "top.png", SimilarScope::All));
    }

    #[test]
    fn scope_folder_keeps_direct_siblings_only() {
        let p = "photos/2024";
        assert!(in_scope(p, "photos/2024/x.png", SimilarScope::Folder));
        assert!(in_scope(p, "photos/2024/y.jpg", SimilarScope::Folder));
        assert!(!in_scope(p, "photos/2024/sub/z.png", SimilarScope::Folder));
        assert!(!in_scope(p, "photos/2023/x.png", SimilarScope::Folder));
        assert!(!in_scope(p, "top.png", SimilarScope::Folder));
    }

    #[test]
    fn scope_folder_at_root_keeps_only_root_level() {
        assert!(in_scope("", "top.png", SimilarScope::Folder));
        assert!(!in_scope("", "sub/x.png", SimilarScope::Folder));
    }

    #[test]
    fn scope_tree_keeps_subtree() {
        let p = "photos/2024";
        assert!(in_scope(p, "photos/2024/x.png", SimilarScope::Tree));
        assert!(in_scope(p, "photos/2024/sub/z.png", SimilarScope::Tree));
        assert!(in_scope(p, "photos/2024/sub/deeper/w.png", SimilarScope::Tree));
        // Sibling directory must NOT match — `photos/2024sub` could
        // collide with a naive prefix check, so the helper uses the
        // `parent/` form.
        assert!(!in_scope(p, "photos/2024sub/x.png", SimilarScope::Tree));
        assert!(!in_scope(p, "photos/2023/x.png", SimilarScope::Tree));
        assert!(!in_scope(p, "top.png", SimilarScope::Tree));
    }

    #[test]
    fn scope_tree_at_root_spans_everything() {
        assert!(in_scope("", "top.png", SimilarScope::Tree));
        assert!(in_scope("", "sub/deep/x.png", SimilarScope::Tree));
    }

    #[test]
    fn scope_parse_unknown_falls_back_to_all() {
        assert_eq!(SimilarScope::parse("folder"), SimilarScope::Folder);
        assert_eq!(SimilarScope::parse("tree"), SimilarScope::Tree);
        assert_eq!(SimilarScope::parse("all"), SimilarScope::All);
        assert_eq!(SimilarScope::parse(""), SimilarScope::All);
        assert_eq!(SimilarScope::parse("garbage"), SimilarScope::All);
    }
}
