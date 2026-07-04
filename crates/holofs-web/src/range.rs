//! HTTP `Range` header parsing for partial `GET` responses.
//!
//! The gateway decodes the whole object into RAM (no progressive decode
//! plumbing yet), but the response can still be a byte slice — that is
//! enough for the workloads HTTP Range actually helps with: resumable
//! large-file downloads, `<audio>` scrubbing, eventual `<video>` seek.
//!
//! Multi-range requests (RFC 9110 §14.1.4) are intentionally not
//! supported. The browser ecosystem rarely uses them and serving them
//! correctly requires `multipart/byteranges` formatting; here we
//! gracefully degrade to a full `200` response instead.

#![cfg(feature = "ssr")]

/// One parsed byte interval. Both bounds are inclusive — RFC 9110
/// `bytes=A-B` carries `B`, the *last* byte index, not the half-open end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end_inclusive: u64,
}

impl ByteRange {
    /// Length in bytes of the slice this range refers to.
    pub fn len(self) -> u64 {
        self.end_inclusive - self.start + 1
    }
}

/// Outcome of parsing a `Range` header against a known `total` body size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    /// Header absent / empty — caller should serve the full body (200).
    NoRange,
    /// Single satisfiable range; caller serves a `206` with the slice.
    Range(ByteRange),
    /// Multi-range request — we fall back to a full `200` instead of
    /// emitting a `multipart/byteranges` body.
    Multiple,
    /// Range syntactically valid but unsatisfiable for this resource
    /// (e.g. start ≥ total). Caller should respond with `416` and a
    /// `Content-Range: bytes */<total>` hint.
    Unsatisfiable,
    /// Malformed header. Same handling as [`NoRange`] per RFC 9110 §14.2 —
    /// servers MAY ignore an unparseable Range and serve the full body.
    Malformed,
}

/// Parse one HTTP `Range` header value (e.g. `"bytes=0-99"`).
///
/// `total` is the full body length in bytes; `0` causes any request to
/// be unsatisfiable.
pub fn parse_range(header: &str, total: u64) -> RangeOutcome {
    let h = header.trim();
    if h.is_empty() {
        return RangeOutcome::NoRange;
    }
    let Some(spec) = h.strip_prefix("bytes=") else {
        return RangeOutcome::Malformed;
    };
    let mut pieces = spec.split(',').map(str::trim).filter(|s| !s.is_empty());
    let first = match pieces.next() {
        Some(p) => p,
        None => return RangeOutcome::Malformed,
    };
    if pieces.next().is_some() {
        return RangeOutcome::Multiple;
    }
    parse_one(first, total)
}

fn parse_one(spec: &str, total: u64) -> RangeOutcome {
    let Some((a, b)) = spec.split_once('-') else {
        return RangeOutcome::Malformed;
    };
    let a = a.trim();
    let b = b.trim();

    if a.is_empty() {
        // Suffix range: `bytes=-N` → last N bytes.
        if b.is_empty() {
            return RangeOutcome::Malformed;
        }
        let suffix = match b.parse::<u64>() {
            Ok(0) => return RangeOutcome::Unsatisfiable,
            Ok(n) => n,
            Err(_) => return RangeOutcome::Malformed,
        };
        if total == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let take = suffix.min(total);
        let start = total - take;
        return RangeOutcome::Range(ByteRange {
            start,
            end_inclusive: total - 1,
        });
    }

    let start = match a.parse::<u64>() {
        Ok(v) => v,
        Err(_) => return RangeOutcome::Malformed,
    };
    if total == 0 || start >= total {
        return RangeOutcome::Unsatisfiable;
    }

    let end_inclusive = if b.is_empty() {
        // `bytes=A-` → from A to end-of-resource.
        total - 1
    } else {
        let raw = match b.parse::<u64>() {
            Ok(v) => v,
            Err(_) => return RangeOutcome::Malformed,
        };
        if raw < start {
            return RangeOutcome::Unsatisfiable;
        }
        raw.min(total - 1)
    };

    RangeOutcome::Range(ByteRange { start, end_inclusive })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: u64, e: u64) -> RangeOutcome {
        RangeOutcome::Range(ByteRange { start: s, end_inclusive: e })
    }

    #[test]
    fn empty_or_missing_header_is_no_range() {
        assert_eq!(parse_range("", 100), RangeOutcome::NoRange);
        assert_eq!(parse_range("   ", 100), RangeOutcome::NoRange);
    }

    #[test]
    fn full_explicit_range() {
        assert_eq!(parse_range("bytes=0-99", 100), r(0, 99));
        assert_eq!(parse_range("bytes=10-20", 100), r(10, 20));
    }

    #[test]
    fn open_ended_range_clamps_to_end() {
        assert_eq!(parse_range("bytes=50-", 100), r(50, 99));
    }

    #[test]
    fn suffix_range_returns_tail() {
        assert_eq!(parse_range("bytes=-25", 100), r(75, 99));
        // Suffix longer than the resource collapses to the whole body.
        assert_eq!(parse_range("bytes=-200", 100), r(0, 99));
    }

    #[test]
    fn range_past_end_clamps() {
        assert_eq!(parse_range("bytes=50-9999", 100), r(50, 99));
    }

    #[test]
    fn unsatisfiable_when_start_ge_total() {
        assert_eq!(parse_range("bytes=100-200", 100), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=200-", 100), RangeOutcome::Unsatisfiable);
    }

    #[test]
    fn unsatisfiable_when_start_after_end() {
        assert_eq!(parse_range("bytes=80-20", 100), RangeOutcome::Unsatisfiable);
    }

    #[test]
    fn empty_resource_anything_unsatisfiable() {
        assert_eq!(parse_range("bytes=0-99", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), RangeOutcome::Unsatisfiable);
    }

    #[test]
    fn multi_range_returns_multiple() {
        assert_eq!(
            parse_range("bytes=0-9,20-29", 100),
            RangeOutcome::Multiple
        );
    }

    #[test]
    fn malformed_inputs() {
        assert_eq!(parse_range("entries=0-9", 100), RangeOutcome::Malformed);
        assert_eq!(parse_range("bytes=abc-9", 100), RangeOutcome::Malformed);
        assert_eq!(parse_range("bytes=0-xyz", 100), RangeOutcome::Malformed);
        assert_eq!(parse_range("bytes=-", 100), RangeOutcome::Malformed);
        assert_eq!(parse_range("bytes=", 100), RangeOutcome::Malformed);
    }

    #[test]
    fn byte_range_len_is_inclusive() {
        assert_eq!(ByteRange { start: 0, end_inclusive: 99 }.len(), 100);
        assert_eq!(ByteRange { start: 50, end_inclusive: 50 }.len(), 1);
    }
}
