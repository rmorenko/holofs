//! Cross-platform disk-space probe for the P1.4b capacity reporter.
//!
//! The node-side `Request::Capacity` handler calls this to fill in the
//! `free_bytes` / `total_bytes` fields of the reply. Delegates to
//! [`fs2`], which wraps `statvfs` on unix and `GetDiskFreeSpaceExW` on
//! windows behind a safe API — a hard requirement here because the
//! workspace lint table sets `unsafe_code = "forbid"`, ruling out
//! direct `libc::statvfs` bindings.
//!
//! # Unknown-capacity contract
//!
//! Any I/O failure (missing dir, permission denied, filesystem that
//! doesn't answer `statvfs`) collapses to `(0, 0)`. The gateway
//! interprets an all-zeros [`crate::node_service::DiskFootprint`] as
//! "capacity unknown, skip skew check for this node", so pathological
//! setups degrade gracefully instead of poisoning the auto-rebalance
//! decision.
//!
//! # Not a hot path
//!
//! Called at most once per node per capacity-poller tick (~60 s in
//! production). Two syscalls per call. No caching needed.

use std::path::Path;

/// Free + total bytes on the mount that hosts `dir`. See the module
/// docstring for the unknown-capacity fallback (`(0, 0)`).
///
/// `dir` doesn't need to exist as a subdirectory of the mount — as
/// long as `dir` itself is a valid directory the underlying
/// `statvfs` picks up the containing mount.
#[must_use]
pub fn free_and_total_bytes(dir: &Path) -> (u64, u64) {
    let free = fs2::available_space(dir).unwrap_or(0);
    let total = fs2::total_space(dir).unwrap_or(0);
    (free, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn free_and_total_are_nonzero_on_tempdir() {
        // Any real filesystem hosting a TempDir must report non-zero
        // total capacity — statvfs failure would surface as (0, 0) via
        // the fallback, and we'd know we regressed the wire.
        let dir = TempDir::new().unwrap();
        let (free, total) = free_and_total_bytes(dir.path());
        assert!(total > 0, "total_bytes should be nonzero on a real fs");
        assert!(
            free <= total,
            "free_bytes ({free}) must not exceed total_bytes ({total})"
        );
    }

    #[test]
    fn missing_path_returns_zeros() {
        // No I/O panic on a bogus path — the reporter degrades to
        // (0, 0) and the gateway skips this node in skew analysis.
        let bogus = Path::new("/definitely/not/a/real/path/holofs-p14b");
        let (free, total) = free_and_total_bytes(bogus);
        assert_eq!((free, total), (0, 0));
    }
}
