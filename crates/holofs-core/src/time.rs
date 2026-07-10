//! Wall-clock helpers. Wraps the `SystemTime::now()` dance that used
//! to be spelled inline in ~12 sites across the workspace (per the
//! S4-3 review finding). Centralised so a future move to a monotonic
//! or injectable clock is a single-file change.

/// Seconds since UNIX epoch. Matches every `created_at_unix` field on
/// disk — clamped to `u64` because negative values (pre-1970) are not
/// representable in that column and would silently underflow the
/// previous `.as_secs()` path.
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Milliseconds since UNIX epoch. Used by the versions store,
/// reputation cache, epoch-GC snapshots, etc.
#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
