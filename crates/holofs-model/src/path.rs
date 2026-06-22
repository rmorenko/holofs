//! Catalog path syntax.
//!
//! A path is a slash-separated sequence of segments — `photos/2026/img.jpg`.
//! Validation is structural and protocol-agnostic; reserved top-level
//! keywords (`api`, `health`, `escrow`, …) stay in `holofs-web` where the
//! HTTP routes live.
//!
//! Rules enforced here:
//! - Path is non-empty.
//! - No leading `/` (paths are root-relative; the catalog has no anchor).
//! - No trailing `/` (one canonical spelling per entry).
//! - No double `/` (no empty segments).
//! - No `.` or `..` segments (path traversal is meaningless inside a
//!   flat-keyed catalog and would create aliasing).
//! - Each segment ≤ 255 bytes (fits POSIX `NAME_MAX`; bounds the on-disk
//!   manifest envelope).
//!
//! Validation returns the input slice unchanged on success — the caller
//! keeps ownership of the original `String`/`&str` and avoids an allocation.

/// Why a path failed validation. Each variant maps to one specific structural
/// rule above; the HTTP layer surfaces them as `400 Bad Request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The empty string.
    Empty,
    /// Starts with `/`.
    LeadingSlash,
    /// Ends with `/`.
    TrailingSlash,
    /// Contains `//` (an empty segment between two separators).
    EmptySegment,
    /// A segment is exactly `.`.
    DotSegment,
    /// A segment is exactly `..`.
    DotDotSegment,
    /// A segment exceeds [`MAX_SEGMENT`] bytes.
    SegmentTooLong,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "path is empty"),
            PathError::LeadingSlash => write!(f, "leading '/' not allowed"),
            PathError::TrailingSlash => write!(f, "trailing '/' not allowed"),
            PathError::EmptySegment => write!(f, "empty segment ('//') not allowed"),
            PathError::DotSegment => write!(f, "'.' segment not allowed"),
            PathError::DotDotSegment => write!(f, "'..' segment not allowed"),
            PathError::SegmentTooLong => {
                write!(f, "segment longer than {MAX_SEGMENT} bytes")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// Maximum bytes per segment. Picked to match POSIX `NAME_MAX` so a
/// hypothetical FUSE adapter can mirror catalog names 1:1 without rewriting.
pub const MAX_SEGMENT: usize = 255;

/// Reject malformed paths; return the input slice unchanged on success.
pub fn validate(path: &str) -> Result<&str, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.starts_with('/') {
        return Err(PathError::LeadingSlash);
    }
    if path.ends_with('/') {
        return Err(PathError::TrailingSlash);
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return Err(PathError::EmptySegment);
        }
        if seg == "." {
            return Err(PathError::DotSegment);
        }
        if seg == ".." {
            return Err(PathError::DotDotSegment);
        }
        if seg.len() > MAX_SEGMENT {
            return Err(PathError::SegmentTooLong);
        }
    }
    Ok(path)
}

/// Parent path (everything up to but excluding the last `/`). `None` when
/// `path` has no separator — i.e. it is a top-level entry whose parent is
/// the implicit root.
///
/// Does **not** revalidate; pass an already-validated path.
pub fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(p, _)| p)
}

/// Last segment of `path` (everything after the last `/`). For top-level
/// paths returns the whole string.
pub fn basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path)
}

/// Iterate over the segments of `path` (split on `/`).
pub fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
}

/// `parent` + `/` + `child`. If `parent` is empty produces just `child` so
/// the result is always relative to the root, never absolute. Caller should
/// pass already-validated components; the join itself does no validation.
pub fn join(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

/// All ancestor prefixes of `path`, root → leaf, excluding `path` itself.
/// For `a/b/c.txt` yields `["a", "a/b"]`. Useful when synthesising
/// missing `Directory` markers during migration.
pub fn ancestors(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = path[start..].find('/') {
        let cut = start + rel;
        out.push(&path[..cut]);
        start = cut + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_simple_paths() {
        assert_eq!(validate("photo.png"), Ok("photo.png"));
        assert_eq!(validate("photos/2026"), Ok("photos/2026"));
        assert_eq!(validate("a/b/c/d.txt"), Ok("a/b/c/d.txt"));
    }

    #[test]
    fn validate_rejects_structural_problems() {
        assert_eq!(validate(""), Err(PathError::Empty));
        assert_eq!(validate("/a"), Err(PathError::LeadingSlash));
        assert_eq!(validate("a/"), Err(PathError::TrailingSlash));
        assert_eq!(validate("a//b"), Err(PathError::EmptySegment));
        assert_eq!(validate("a/./b"), Err(PathError::DotSegment));
        assert_eq!(validate("a/../b"), Err(PathError::DotDotSegment));
        assert_eq!(validate(".."), Err(PathError::DotDotSegment));
        assert_eq!(validate("."), Err(PathError::DotSegment));
        let long = "a".repeat(MAX_SEGMENT + 1);
        assert_eq!(validate(&long), Err(PathError::SegmentTooLong));
    }

    #[test]
    fn parent_and_basename() {
        assert_eq!(parent("a/b/c"), Some("a/b"));
        assert_eq!(parent("top"), None);
        assert_eq!(basename("a/b/c.txt"), "c.txt");
        assert_eq!(basename("solo"), "solo");
    }

    #[test]
    fn join_combines_or_skips_root() {
        assert_eq!(join("a/b", "c"), "a/b/c");
        assert_eq!(join("", "top"), "top");
    }

    #[test]
    fn ancestors_walks_root_to_parent() {
        assert_eq!(ancestors("a/b/c.txt"), vec!["a", "a/b"]);
        assert_eq!(ancestors("top"), Vec::<&str>::new());
        assert_eq!(ancestors("a/b"), vec!["a"]);
    }

    #[test]
    fn segments_yields_pieces() {
        let s: Vec<&str> = segments("a/b/c").collect();
        assert_eq!(s, vec!["a", "b", "c"]);
    }
}
