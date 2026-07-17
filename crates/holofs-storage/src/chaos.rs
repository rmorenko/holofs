//! Test-only fault injection for chaos scenarios (P1.6).
//!
//! # Why
//!
//! Some failure modes — disk-full on WAL append, disk-full at fsync,
//! long tail latency on kernel writeback — are hostile to trigger from
//! a real filesystem in CI: they need root, a scratch device, or an
//! entire second disk. The chaos module lets integration tests arm a
//! process-local flag that the WAL write path checks and turns into a
//! synthetic `io::Error` at the appropriate moment.
//!
//! # Off by default
//!
//! Everything here is guarded by the `chaos` crate feature. The
//! release build never compiles the flags — the two hook functions
//! (`maybe_fail_wal_append`, `maybe_fail_wal_sync`) collapse to
//! `Ok(())` inline via `#[cfg]`, so hot-path callers pay zero cost.
//! CI runs a dedicated `cargo test -p holofs-storage --features
//! chaos` step to cover these paths.
//!
//! # Model
//!
//! Faults are *one-shot* by default: `arm_wal_next_append_fails(...)`
//! sets a flag, the next `WalWriter::write_record` sees the flag,
//! clears it, and returns the synthetic error. This mirrors real disk
//! full more closely than a persistent fault — the second write after
//! the disk is freed should succeed, and tests should verify that
//! recovery.
//!
//! When the feature is off, the arm/disarm helpers are still defined
//! (they just no-op) so test code guarded by `#[cfg(feature =
//! "chaos")]` doesn't need mirror stubs in the crate-under-test.

#[cfg(feature = "chaos")]
use std::io;
#[cfg(feature = "chaos")]
use std::sync::atomic::{AtomicI32, Ordering};

/// Sentinel meaning "no fault armed". Chosen outside the range of
/// `io::ErrorKind::*` raw values so it never collides with a real
/// injection request.
#[cfg(feature = "chaos")]
const NO_FAULT: i32 = 0;

/// One-shot fault for the next `WalWriter::write_record` call. Stores
/// an `io::ErrorKind` discriminant as `i32`; the append hook clears
/// the slot back to `NO_FAULT` after firing.
#[cfg(feature = "chaos")]
static WAL_APPEND_FAULT: AtomicI32 = AtomicI32::new(NO_FAULT);

/// One-shot fault for the next `WalWriter::sync` call. Separate from
/// the append fault so tests can distinguish "buffered write refused"
/// from "kernel refused to fsync" — the two failure modes exercise
/// different code paths in higher-level callers.
#[cfg(feature = "chaos")]
static WAL_SYNC_FAULT: AtomicI32 = AtomicI32::new(NO_FAULT);

/// Encode an `io::ErrorKind` into the packed `i32` slot format used
/// by the atomic fault registers. Using an explicit mapping (instead
/// of `as i32`) means the value survives std reshuffles of the
/// ErrorKind enum and lets us reject `NO_FAULT`'s sentinel explicitly.
#[cfg(feature = "chaos")]
fn encode_kind(kind: io::ErrorKind) -> i32 {
    match kind {
        io::ErrorKind::StorageFull => 1,
        io::ErrorKind::OutOfMemory => 2,
        io::ErrorKind::WriteZero => 3,
        io::ErrorKind::PermissionDenied => 4,
        io::ErrorKind::BrokenPipe => 5,
        io::ErrorKind::Other => 6,
        _ => 6,
    }
}

#[cfg(feature = "chaos")]
fn decode_kind(raw: i32) -> io::ErrorKind {
    match raw {
        1 => io::ErrorKind::StorageFull,
        2 => io::ErrorKind::OutOfMemory,
        3 => io::ErrorKind::WriteZero,
        4 => io::ErrorKind::PermissionDenied,
        5 => io::ErrorKind::BrokenPipe,
        _ => io::ErrorKind::Other,
    }
}

/// Arm a one-shot fault on the next WAL append. No-op when the
/// `chaos` feature is disabled.
#[cfg(feature = "chaos")]
pub fn arm_wal_next_append_fails(kind: io::ErrorKind) {
    WAL_APPEND_FAULT.store(encode_kind(kind), Ordering::SeqCst);
}

/// Off-feature stub so test binaries can reference the same call
/// site regardless of feature flag. When `chaos` is off, tests that
/// arm faults are compiled out along with the arming call, but
/// keeping this symbol lets `#[cfg]`-less docs and macros compile.
#[cfg(not(feature = "chaos"))]
#[allow(dead_code)]
pub fn arm_wal_next_append_fails(_kind: std::io::ErrorKind) {
    // no-op: chaos feature disabled
}

/// Arm a one-shot fault on the next WAL sync/fsync. See
/// [`arm_wal_next_append_fails`] for shape.
#[cfg(feature = "chaos")]
pub fn arm_wal_next_sync_fails(kind: io::ErrorKind) {
    WAL_SYNC_FAULT.store(encode_kind(kind), Ordering::SeqCst);
}

/// Off-feature stub. See [`arm_wal_next_append_fails`] for the
/// symmetric behaviour on the feature-on side.
#[cfg(not(feature = "chaos"))]
#[allow(dead_code)]
pub fn arm_wal_next_sync_fails(_kind: std::io::ErrorKind) {
    // no-op: chaos feature disabled
}

/// Clear both armed faults. Tests should call this in a teardown
/// guard because the atomics are process-global — if a test panics
/// with a fault armed, the next unrelated test in the same process
/// (rare in Rust's per-test-binary model, but possible with `--test-
/// threads`) would inherit the fault.
#[cfg(feature = "chaos")]
pub fn disarm_all() {
    WAL_APPEND_FAULT.store(NO_FAULT, Ordering::SeqCst);
    WAL_SYNC_FAULT.store(NO_FAULT, Ordering::SeqCst);
}

/// Off-feature stub. See [`disarm_all`] for the symmetric feature-on
/// behaviour that clears both fault registers.
#[cfg(not(feature = "chaos"))]
#[allow(dead_code)]
pub fn disarm_all() {
    // no-op: chaos feature disabled
}

/// Hook called at the top of `WalWriter::write_record`. Returns
/// `Some(err)` iff a one-shot append fault is armed; the fault is
/// cleared before returning so the next call succeeds naturally.
///
/// With the `chaos` feature off the function is `#[inline(always)]`
/// and hard-returns `None`, so LLVM erases the call site entirely.
#[cfg(feature = "chaos")]
#[must_use]
pub fn maybe_fail_wal_append() -> Option<std::io::Error> {
    let raw = WAL_APPEND_FAULT.swap(NO_FAULT, Ordering::SeqCst);
    if raw == NO_FAULT {
        None
    } else {
        Some(std::io::Error::new(
            decode_kind(raw),
            "chaos: injected WAL append fault",
        ))
    }
}

/// Off-feature stub for [`maybe_fail_wal_append`]. Always returns
/// `None` so LLVM erases the call site in release builds. The
/// `#[inline(always)]` is deliberate — even in unoptimised builds
/// there's no function-call overhead added to the WAL hot path.
#[cfg(not(feature = "chaos"))]
#[inline(always)]
#[must_use]
pub fn maybe_fail_wal_append() -> Option<std::io::Error> {
    None
}

/// Hook called at the top of `WalWriter::sync` (after buffered flush,
/// before fsync). Same semantics as [`maybe_fail_wal_append`].
#[cfg(feature = "chaos")]
#[must_use]
pub fn maybe_fail_wal_sync() -> Option<std::io::Error> {
    let raw = WAL_SYNC_FAULT.swap(NO_FAULT, Ordering::SeqCst);
    if raw == NO_FAULT {
        None
    } else {
        Some(std::io::Error::new(
            decode_kind(raw),
            "chaos: injected WAL sync fault",
        ))
    }
}

/// Off-feature stub for [`maybe_fail_wal_sync`]. See
/// [`maybe_fail_wal_append`] for the same zero-cost inline shape.
#[cfg(not(feature = "chaos"))]
#[inline(always)]
#[must_use]
pub fn maybe_fail_wal_sync() -> Option<std::io::Error> {
    None
}

#[cfg(all(test, feature = "chaos"))]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn append_fault_fires_once_then_clears() {
        disarm_all();
        assert!(maybe_fail_wal_append().is_none(), "starts disarmed");
        arm_wal_next_append_fails(ErrorKind::StorageFull);
        let e = maybe_fail_wal_append().expect("armed → Some");
        assert_eq!(e.kind(), ErrorKind::StorageFull);
        assert!(
            maybe_fail_wal_append().is_none(),
            "single arm should be one-shot"
        );
    }

    #[test]
    fn sync_fault_is_independent_of_append_fault() {
        disarm_all();
        arm_wal_next_sync_fails(ErrorKind::BrokenPipe);
        assert!(
            maybe_fail_wal_append().is_none(),
            "append fault register untouched by sync arm"
        );
        let e = maybe_fail_wal_sync().expect("sync armed → Some");
        assert_eq!(e.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn disarm_all_clears_both() {
        arm_wal_next_append_fails(ErrorKind::StorageFull);
        arm_wal_next_sync_fails(ErrorKind::StorageFull);
        disarm_all();
        assert!(maybe_fail_wal_append().is_none());
        assert!(maybe_fail_wal_sync().is_none());
    }
}
