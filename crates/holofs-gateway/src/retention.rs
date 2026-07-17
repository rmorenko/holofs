//! P2.2 — per-object retention policy management + background GC.
//!
//! # What lives here
//!
//! - [`Gateway::set_retention`] — write a [`RetentionPolicy`] onto a
//!   catalog manifest, or clear it (pass `None`). CAS-write-back
//!   guards against concurrent PUT-replace so a fresh PUT's retention
//!   wins over a stale one.
//! - [`Gateway::get_retention`] — read the current policy.
//! - [`Gateway::gc_expired_objects_tick`] — one sweep of the catalog.
//!   Deletes every non-directory object whose retention policy has
//!   tripped by the given wall-clock. Returns per-object outcome so
//!   handlers / tests can assert what fired.
//! - [`spawn_retention_gc`] — bootstrap-side helper that ticks the
//!   catalog on a fixed interval (default 1 h, env
//!   `HOLOFS_RETENTION_GC_INTERVAL_SECS`, `0` disables).
//!
//! # Design notes
//!
//! The policy lives *in the manifest* (not in a side table) so it
//! travels with backup/restore + object-level export. That means any
//! catalog mutation that rewrites the manifest (PUT, drain-node
//! rebalance, add-node repair) risks losing an existing retention
//! setting. Rule of thumb inside the mutation callers: if you're
//! taking a manifest, editing it, and writing it back, PRESERVE the
//! `retention` field. Every current mutation path does this
//! implicitly because they `.clone()` the manifest and mutate only
//! specific fields — but a new mutation caller adding a field-by-
//! field constructor must remember to copy `retention` too.
//!
//! # Rate limiting
//!
//! The GC daemon caps how many objects it deletes per tick
//! ([`RETENTION_GC_MAX_PER_TICK`]) so a cluster with a huge burst of
//! expiries doesn't hold the catalog write lock for minutes and stall
//! every concurrent PUT. Unfinished expiries roll over to the next
//! tick.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use holofs_model::fs::Directory;
use holofs_model::manifest::{Manifest, ObjectKind, RetentionPolicy};
use holofs_model::path as catalog_path;

use crate::error::GatewayError;
use crate::Gateway;

/// Default retention-GC tick period — every 60 minutes. Overridable
/// at boot via `HOLOFS_RETENTION_GC_INTERVAL_SECS`; `0` disables the
/// daemon entirely (retention policies can still be set, but nothing
/// deletes on their expiry).
pub const DEFAULT_RETENTION_GC_INTERVAL_SECS: u64 = 3600;

/// Ceiling on deletions per tick. On a huge burst of expiries the
/// remainder rolls over to the next tick, keeping any single sweep
/// bounded so the catalog write lock never stalls for minutes.
pub const RETENTION_GC_MAX_PER_TICK: usize = 512;

/// Per-object outcome inside a [`Gateway::gc_expired_objects_tick`]
/// return value. Kept pub(crate) for now — the shape may grow
/// (retention-kind, delta-since-expiry) as follow-up policies land,
/// and there's no external consumer beyond the daemon + tests yet.
#[derive(Debug, Clone)]
pub struct RetentionGcOutcome {
    /// Catalog name that had its retention trip.
    pub name: String,
    /// Was `remove_object` successful? `false` on error (see `error`).
    pub deleted: bool,
    /// Human-readable error when `deleted == false`.
    pub error: Option<String>,
}

/// Aggregate result of a single [`Gateway::gc_expired_objects_tick`].
#[derive(Debug, Clone, Default)]
pub struct RetentionGcReport {
    /// Wall-clock the tick used to compare against every policy's
    /// deadline. Snapshotted once at tick start so a slow scan
    /// doesn't leak "now" into the middle of the walk.
    pub now_unix: u64,
    /// Every non-directory object whose retention policy tripped.
    pub outcomes: Vec<RetentionGcOutcome>,
    /// `true` when the tick stopped early due to
    /// [`RETENTION_GC_MAX_PER_TICK`]. Callers looking at this can
    /// decide whether to re-run immediately or wait for the next
    /// scheduled tick.
    pub hit_per_tick_cap: bool,
}

impl Gateway {
    /// Set or clear a [`RetentionPolicy`] on the catalog entry
    /// `name`. Passing `None` clears an existing policy. The manifest
    /// is deep-cloned, mutated, and written back under the catalog
    /// write lock with a `data_cid` CAS so a concurrent PUT-replace's
    /// fresh manifest is preserved intact.
    ///
    /// # Errors
    /// - [`GatewayError::BadRequest`] — invalid catalog path.
    /// - [`GatewayError::NotFound`] — `name` not in catalog.
    /// - [`GatewayError::IsDirectory`] — target is a directory entry
    ///   (directories can't expire; use `rmdir` instead).
    pub async fn set_retention(
        &self,
        name: &str,
        policy: Option<RetentionPolicy>,
    ) -> Result<(), GatewayError> {
        catalog_path::validate(name).map_err(|e| GatewayError::BadRequest(e.to_string()))?;

        // Snapshot the current manifest under a short read lock.
        let snapshot = {
            let cat = self.catalog.read().await;
            cat.entries.get(name).cloned()
        };
        let Some(snap) = snapshot else {
            return Err(GatewayError::NotFound);
        };
        if snap.kind == ObjectKind::Directory {
            return Err(GatewayError::IsDirectory);
        }
        let snap_data_cid = snap.data_cid;
        let mut mutable: Manifest = (*snap).clone();
        drop(snap);

        mutable.retention = policy;

        // CAS write-back: only apply if the catalog entry's data_cid
        // still matches the snapshot. A concurrent PUT-replace under
        // us has already produced a fresh manifest with its own
        // (possibly-different, possibly-None) retention — trampling
        // it would be a silent policy drop.
        let applied = {
            let mut cat = self.catalog.write().await;
            let cur_cid = cat.entries.get(name).map(|m| m.data_cid);
            if cur_cid == Some(snap_data_cid) {
                cat.insert(name.to_string(), mutable);
                true
            } else {
                false
            }
        };
        if !applied {
            // Concurrent PUT-replace won the race. Report success
            // to the caller with `NotFound` — the retention they
            // wanted to attach targets a manifest that no longer
            // exists. Callers can retry against the fresh manifest.
            return Err(GatewayError::NotFound);
        }
        self.mark_catalog_dirty(name).await;
        self.persist_catalog().await?;
        Ok(())
    }

    /// Read the current retention policy for `name`. `Ok(None)` = no
    /// policy set (default). Errors identically to
    /// [`Self::set_retention`] modulo the write-side codes.
    pub async fn get_retention(&self, name: &str) -> Result<Option<RetentionPolicy>, GatewayError> {
        catalog_path::validate(name).map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let cat = self.catalog.read().await;
        let m = cat.entries.get(name).ok_or(GatewayError::NotFound)?;
        if m.kind == ObjectKind::Directory {
            return Err(GatewayError::IsDirectory);
        }
        Ok(m.retention)
    }

    /// One sweep of the catalog: find every non-directory manifest
    /// whose retention policy has tripped by `now_unix` and delete
    /// them via [`Self::remove_object`]. Bounded by
    /// [`RETENTION_GC_MAX_PER_TICK`] — surplus expiries roll over.
    ///
    /// `now_unix` is passed in rather than computed inside so the
    /// tests can drive expiry deterministically without waiting on
    /// wall clock.
    pub async fn gc_expired_objects_tick(&self, now_unix: u64) -> RetentionGcReport {
        // Snapshot the candidate names under a short read lock so we
        // don't hold the write lock across per-name `remove_object`
        // calls (which themselves take the write lock).
        let candidates: Vec<String> = {
            let cat = self.catalog.read().await;
            cat.entries
                .iter()
                .filter(|(_, m)| m.kind != ObjectKind::Directory)
                .filter_map(|(name, m)| {
                    m.retention
                        .filter(|p| p.is_expired(now_unix))
                        .map(|_| name.clone())
                })
                .collect()
        };

        let mut report = RetentionGcReport {
            now_unix,
            outcomes: Vec::with_capacity(candidates.len().min(RETENTION_GC_MAX_PER_TICK)),
            hit_per_tick_cap: false,
        };
        for (i, name) in candidates.into_iter().enumerate() {
            if i >= RETENTION_GC_MAX_PER_TICK {
                report.hit_per_tick_cap = true;
                break;
            }
            match self.remove_object(&name).await {
                Ok(_) => report.outcomes.push(RetentionGcOutcome {
                    name,
                    deleted: true,
                    error: None,
                }),
                Err(e) => report.outcomes.push(RetentionGcOutcome {
                    name,
                    deleted: false,
                    error: Some(e.to_string()),
                }),
            }
        }
        report
    }
}

/// Spawn the retention-GC background task. Ticks every
/// `interval_secs`; on each tick calls
/// [`Gateway::gc_expired_objects_tick`] with the current wall clock.
/// Returns the join handle so bootstrap can hold it for lifetime
/// purposes. `interval_secs = 0` disables the daemon entirely and
/// returns `None`.
pub fn spawn_retention_gc(
    gateway: Arc<Gateway>,
    _catalog: Arc<RwLock<Directory>>,
    interval_secs: u64,
) -> Option<JoinHandle<()>> {
    if interval_secs == 0 {
        return None;
    }
    let interval = Duration::from_secs(interval_secs.max(60));
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately — skip it so a fresh boot
        // doesn't shove a retention sweep before the catalog is
        // fully warm.
        tick.tick().await;
        loop {
            tick.tick().await;
            let now_unix = holofs_core::time::now_unix_ms() / 1000;
            let report = gateway.gc_expired_objects_tick(now_unix).await;
            if !report.outcomes.is_empty() || report.hit_per_tick_cap {
                let deleted = report.outcomes.iter().filter(|o| o.deleted).count();
                let failed = report.outcomes.len() - deleted;
                tracing::info!(
                    now_unix,
                    deleted,
                    failed,
                    hit_cap = report.hit_per_tick_cap,
                    "retention GC tick complete"
                );
            }
        }
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    // Unit tests for the daemon policy live alongside the gateway
    // integration tests in `crates/holofs-gateway/tests/retention.rs`
    // — they need a real Gateway (catalog + persistence) to exercise
    // the CAS-write-back and remove_object plumbing end-to-end, and
    // that harness already exists there.
}
