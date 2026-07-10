//! per-object version history.
//!
//! On-disk layout: `<versions_root>/versions/<sanitized_name>/v<ts_ms>_<cid8>.bin`.
//! Each `.bin` is a serialised [`Manifest`](holofs_model::manifest::Manifest)
//! of a prior version. On PUT-replace, the prior manifest is archived
//! (shard purge skipped) so a `restore_version` call finds its shards
//! still alive on the cluster.
//!

use holofs_core::hash::hex;
use holofs_model::manifest::{Manifest, ObjectKind};

use crate::error::GatewayError;
use crate::Gateway;


/// Lazy versioning state. `enabled` set via `Gateway::enable_versions`;
/// the on-disk layout is `root/<sanitized_name>/v<ts_ms>_<cid8>.bin`,
/// each file holding one `Manifest::encode()` of a prior version.
#[derive(Default)]
pub(crate) struct VersionsState {
    pub(crate) enabled: bool,
    pub(crate) root: Option<std::path::PathBuf>,
    /// Retention cap: when set, `archive_version` prunes the oldest
    /// versions until at most `keep_last` remain (the newly-archived
    /// one counts). `None` (default) means unlimited — versions
    /// accumulate forever until manually deleted via `delete_version`.
    pub(crate) keep_last: Option<usize>,
}

/// One row of [`Gateway::list_versions`].
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Opaque id used in restore — the filename minus `.bin`. URL-safe.
    pub id: String,
    /// Unix ms when the version was archived (== time the *next* PUT
    /// for this name landed). Drives the human-readable timestamp.
    pub created_at_ms: u64,
    /// First 16 hex chars of the manifest's `data_cid`. Lets the user
    /// confirm a version is the one they're looking for without
    /// scrolling the whole hash.
    pub cid_short: String,
    /// `width × height` for image / `samples × 1` for audio — same
    /// rendering as on the catalog row.
    pub width: u32,
    pub height: u32,
    /// Kind so the UI can pick the right thumbnail strategy.
    pub kind: ObjectKind,
}
impl Gateway {
    /// Path to the directory holding version side files for `name`.
    /// Sanitisation: directory separators in the catalog name become
    /// `__` so each object gets a flat folder under `root`.
    fn version_dir_for(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let safe = name.replace('/', "__").replace(['\\', ':', '?', '*', '"', '<', '>', '|'], "_");
        root.join("versions").join(safe)
    }

    /// Sanitise / build the path for a single version file.
    fn version_file_for(
        root: &std::path::Path,
        name: &str,
        ts_ms: u64,
        cid: &[u8; 32],
    ) -> std::path::PathBuf {
        let cid_short: String = cid.iter().take(4).map(|b| format!("{b:02x}")).collect();
        Self::version_dir_for(root, name).join(format!("v{ts_ms}_{cid_short}.bin"))
    }

    /// Belt-and-braces path guard for the version routes. After
    /// [`Self::validate_version_id`] the id can no longer contain a
    /// `..` or `/`, but if the format ever grows we want the property
    /// checked at the filesystem layer too: the joined path must live
    /// under the per-name version dir.
    fn assert_inside(
        dir: &std::path::Path,
        path: &std::path::Path,
    ) -> Result<(), GatewayError> {
        // Canonical form only exists once the dir has been created,
        // so fall back to lexical containment when either canonical
        // resolution fails (e.g. dir not yet materialised).
        let (canon_dir, canon_path) = match (dir.canonicalize(), path.canonicalize()) {
            (Ok(d), Ok(p)) => (d, p),
            _ => (dir.to_path_buf(), path.to_path_buf()),
        };
        if canon_path.starts_with(&canon_dir) {
            Ok(())
        } else {
            Err(GatewayError::BadRequest(
                "version id resolves outside versions root".into(),
            ))
        }
    }

    /// Validate a version id passed on a mutating unauthenticated route.
    /// The on-disk format is `v<ts_ms>_<cid8>.bin` (`version_file_for`),
    /// so a legit id is `^v\d+_[0-9a-f]{8}$`. Anything else (`../…`,
    /// `%2e%2e`, absolute paths) is rejected — the router treats
    /// `restore_version` / `delete_version` as unauthenticated medium
    /// routes, so passing raw `id` straight into `dir.join(...)` would
    /// let anyone read/delete arbitrary `*.bin` files outside the
    /// versions root.
    fn validate_version_id(id: &str) -> Result<(), GatewayError> {
        let ok = id
            .strip_prefix('v')
            .and_then(|r| r.split_once('_'))
            .map(|(ts, cid)| {
                !ts.is_empty()
                    && ts.bytes().all(|b| b.is_ascii_digit())
                    && cid.len() == 8
                    && cid.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(GatewayError::BadRequest(format!(
                "bad version id: {id:?} (expected v<ts_ms>_<cid8>)"
            )))
        }
    }

    /// Archive a manifest to the versions side store. Called from
    /// `ingest_bytes` BEFORE the catalog mutation and the shard purge
    /// (which we then skip for the prior shards). Cheap — just one
    /// encode + one file write.
    pub(crate) async fn archive_version(&self, name: &str, manifest: &Manifest) -> Result<(), GatewayError> {
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Ok(());
        };
        drop(s);
        let dir = Self::version_dir_for(&root, name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| GatewayError::BadRequest(format!("versions mkdir: {e}")))?;
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let path = Self::version_file_for(&root, name, ts_ms, &manifest.data_cid);
        let bytes = manifest.encode();
        std::fs::write(&path, &bytes)
            .map_err(|e| GatewayError::BadRequest(format!("versions write: {e}")))?;
        // Trim oldest archives if a retention cap is configured. No
        // gc_barrier needed anymore (epoch-GC) — the shards
        // referenced by any archive still live in the store; if
        // concurrent full GC's snapshot froze before the archive
        // write, its per-node PurgeByHashUpTo won't touch our new
        // shards (they carry post-snapshot epochs).
        self.prune_versions_to_cap(name).await;
        Ok(())
    }

    /// List archived versions of `name`, newest first. Returns an
    /// empty vec when versioning is off or no versions exist.
    pub async fn list_versions(
        &self,
        name: &str,
    ) -> Result<Vec<VersionEntry>, GatewayError> {
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Ok(Vec::new());
        };
        drop(s);
        let dir = Self::version_dir_for(&root, name);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<VersionEntry> = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| GatewayError::BadRequest(format!("versions readdir: {e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // `vTS_CID.bin` → parse.
            let stem = match file_name.strip_suffix(".bin") {
                Some(s) => s,
                None => continue,
            };
            let after_v = match stem.strip_prefix('v') {
                Some(s) => s,
                None => continue,
            };
            let (ts, cid_short) = match after_v.split_once('_') {
                Some((ts, cid)) => (ts.parse::<u64>().unwrap_or(0), cid.to_string()),
                None => continue,
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let manifest = match Manifest::decode(&bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(VersionEntry {
                id: stem.to_string(),
                created_at_ms: ts,
                cid_short,
                width: manifest.width,
                height: manifest.height,
                kind: manifest.kind,
            });
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at_ms));
        Ok(out)
    }

    /// Swap the catalog entry for `name` with the archived version
    /// `id`. The currently-live manifest is archived first so the
    /// swap is reversible (it appears as a fresh version with the
    /// current timestamp). Old shards stay on cluster nodes —
    /// versioning treats every version as immutable.
    pub async fn restore_version(
        &self,
        name: &str,
        id: &str,
    ) -> Result<RestoreResult, GatewayError> {
        // epoch-GC: no gc_barrier here. Any shards the target
        // manifest references are already on the cluster — GC's
        // orphan-set computation is a snapshot, and the shards it
        // references trace back to a live catalog or archived
        // manifest. If a concurrent GC pass purges anything, it
        // won't be ours: the archived shards' epochs predate the
        // GC snapshot only if THEIR objects were dropped from the
        // catalog + archive before the snapshot, which is exactly
        // the definition of orphan.
        Self::validate_version_id(id)?;
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Err(GatewayError::BadRequest(
                "versions disabled — restart with --enable-versions".into(),
            ));
        };
        drop(s);
        // Locate the version file by id.
        let dir = Self::version_dir_for(&root, name);
        let path = dir.join(format!("{id}.bin"));
        Self::assert_inside(&dir, &path)?;
        if !path.exists() {
            return Err(GatewayError::NotFound);
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| GatewayError::BadRequest(format!("versions read: {e}")))?;
        let target = Manifest::decode(&bytes)
            .map_err(|e| GatewayError::BadRequest(format!("versions decode: {e}")))?;

        // Archive the current manifest before replacing it. If the
        // name isn't in the catalog at all (was deleted), restore
        // becomes a pure resurrection — no prior to archive.
        let current = self.catalog.read().await.get(name).cloned();
        if let Some(prev) = &current {
            self.archive_version(name, prev).await?;
        }

        let restored_cid = hex(&target.data_cid);
        self.catalog
            .write()
            .await
            .insert(name.to_string(), target);
        self.invalidate_cache(name).await;
        self.persist_catalog().await?;
        Ok(RestoreResult {
            name: name.to_string(),
            restored_cid_hex: restored_cid,
        })
    }

    /// Permanently delete an archived version of `name`. The on-disk
    /// `.bin` is removed and any shards it uniquely held (not
    /// referenced by the current catalog entry nor by any other
    /// surviving archive) are GC'd from the cluster.
    ///
    /// Returns `NotFound` if the version id doesn't exist. Returns
    /// `BadRequest` if versions are disabled.
    pub async fn delete_version(
        &self,
        name: &str,
        id: &str,
    ) -> Result<DeleteVersionResult, GatewayError> {
        // epoch-GC: no gc_barrier here. Double-purge with a
        // concurrent full-GC pass is idempotent (purge_orphans_of
        // and PurgeByHashUpTo both no-op on missing hashes); the
        // orphan-diff race isn't observable to callers because
        // `delete_version_inner` computes its orphan set from a
        // fresh catalog+archive snapshot inside its own critical
        // section.
        self.delete_version_inner(name, id).await
    }

    /// Same as `delete_version` — kept as a distinct entry point
    /// only because `prune_versions_to_cap` used to need a "no
    /// barrier acquire" version. Now they're identical but the
    /// two-name pattern documents the two call paths.
    async fn delete_version_inner(
        &self,
        name: &str,
        id: &str,
    ) -> Result<DeleteVersionResult, GatewayError> {
        Self::validate_version_id(id)?;
        let s = self.versions.lock().await;
        let Some(root) = s.root.clone() else {
            return Err(GatewayError::BadRequest(
                "versions disabled — start with --enable-versions".into(),
            ));
        };
        drop(s);
        let dir = Self::version_dir_for(&root, name);
        let path = dir.join(format!("{id}.bin"));
        Self::assert_inside(&dir, &path)?;
        if !path.exists() {
            return Err(GatewayError::NotFound);
        }
        // Read + decode the manifest BEFORE deleting so we know
        // which shard hashes were tied to this version. If decode
        // fails (corrupt file) we still drop the file — better to
        // free disk space than leave a poisoned archive entry.
        let bytes = std::fs::read(&path)
            .map_err(|e| GatewayError::BadRequest(format!("versions read: {e}")))?;
        let manifest = Manifest::decode(&bytes).ok();
        std::fs::remove_file(&path)
            .map_err(|e| GatewayError::BadRequest(format!("versions delete: {e}")))?;
        // Best-effort orphan GC. purge_orphans_of walks catalog +
        // the remaining version archives on disk; the file we just
        // deleted will not show up in that walk, so its uniquely-
        // owned shards become the orphan set.
        let mut shards_purged = 0u64;
        if let Some(m) = manifest {
            let live = self.effective_live().await;
            if !live.is_empty() {
                // Count owned hashes before purge for the report.
                let mut owned: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
                for chan in &m.shard_hashes {
                    for per_l in chan {
                        for h in per_l {
                            owned.insert(*h);
                        }
                    }
                }
                shards_purged = owned.len() as u64;
                if let Err(e) = self.purge_orphans_of(&m, &live, None).await {
                    eprintln!("delete_version {name} {id}: orphan purge failed (continuing): {e}");
                }
            }
        }
        Ok(DeleteVersionResult {
            name: name.to_string(),
            id: id.to_string(),
            shards_owned: shards_purged,
        })
    }

    /// Prune the oldest archives beyond `keep_last` for `name`.
    /// Called from `archive_version` right after a new version file
    /// is written. No-op when retention is unset or the current
    /// archive count is at or below the cap.
    async fn prune_versions_to_cap(&self, name: &str) {
        let (root, cap) = {
            let s = self.versions.lock().await;
            match (s.root.clone(), s.keep_last) {
                (Some(r), Some(n)) => (r, n),
                _ => return,
            }
        };
        let dir = Self::version_dir_for(&root, name);
        if !dir.exists() {
            return;
        }
        // Sort archive files by mtime-encoded timestamp in filename
        // (`vTS_CID.bin`). Newest first; anything past `cap` gets dropped.
        let mut entries: Vec<(u64, String)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in rd.flatten() {
            let Some(file_name) = e.path().file_name().and_then(|s| s.to_str()).map(str::to_string)
            else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".bin") else {
                continue;
            };
            let Some(after_v) = stem.strip_prefix('v') else {
                continue;
            };
            let Some((ts, _)) = after_v.split_once('_') else {
                continue;
            };
            let Ok(ts) = ts.parse::<u64>() else {
                continue;
            };
            entries.push((ts, stem.to_string()));
        }
        if entries.len() <= cap {
            return;
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        let to_drop: Vec<String> = entries.into_iter().skip(cap).map(|(_, id)| id).collect();
        for id in to_drop {
            if let Err(e) = self.delete_version_inner(name, &id).await {
                eprintln!("prune_versions_to_cap {name} {id}: {e}");
            }
        }
    }
}


/// Result of [`Gateway::restore_version`].
#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub name: String,
    /// Hex `data_cid` of the now-live manifest after the swap.
    pub restored_cid_hex: String,
}

/// Result of [`Gateway::delete_version`]. `shards_owned` is the count
/// of shard hashes the deleted manifest claimed — most of those will
/// have been GC'd from the cluster (some may have survived if other
/// archives still reference them).
#[derive(Debug, Clone)]
pub struct DeleteVersionResult {
    pub name: String,
    pub id: String,
    pub shards_owned: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_version_id_accepts_canonical() {
        assert!(Gateway::validate_version_id("v1712345678000_deadbeef").is_ok());
        assert!(Gateway::validate_version_id("v0_00000000").is_ok());
    }

    #[test]
    fn validate_version_id_rejects_path_traversal() {
        // The exact strings a malicious client would send.
        for bad in [
            "../../catalog",
            "..%2F..%2Fcatalog",
            "v../abcdef01",
            "v1234_../defghij",
            "v1234_/passwd",
            "vabc_deadbeef",       // ts_ms must be digits
            "v1712345678000_ZZZZZZZZ", // cid8 must be hex
            "v1712345678000_dead",  // cid8 must be exactly 8 hex
            "v1712345678000_deadbeef1", // cid8 too long
            "",
            "v",
            "v1712345678000",  // no _cid
            "v_deadbeef",       // no ts
        ] {
            assert!(
                Gateway::validate_version_id(bad).is_err(),
                "expected reject for {bad:?}"
            );
        }
    }
}

