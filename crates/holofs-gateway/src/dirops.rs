//! Catalog directory / entry mutations — the CRUD verbs that don't
//! touch the coding path.
//!
//! - [`Gateway::remove_object`] — DELETE for a data object (not a
//!   directory; use `rmdir` for those). Purges only the shards
//!   unique to the manifest so shared-`data_cid` neighbours stay
//!   decodeable.
//! - [`Gateway::mkdir`] — create a `Directory` catalog entry.
//! - [`Gateway::rmdir`] — remove an empty directory.
//! - [`Gateway::rename`] — atomic rename; directory renames rewrite
//!   every descendant path.
//! - [`Gateway::list_dir`] — immediate-children listing.
//!

use holofs_model::manifest::{Manifest, ObjectKind};
use holofs_model::path as catalog_path;

use crate::error::GatewayError;
use crate::util::{directory_object_id, now_unix};
use crate::Gateway;

/// Summary of a successful DELETE.
#[derive(Debug, Clone)]
pub struct RemoveResult {
    /// Catalog name that was removed.
    pub name: String,
    /// 64-bit `object_id` whose shards were Purged.
    pub object_id: u64,
}

/// Summary of a successful `mkdir`.
#[derive(Debug, Clone)]
pub struct MkdirResult {
    /// Catalog path of the new directory entry.
    pub path: String,
    /// Stable directory object id (SHA-256-derived from the path).
    pub object_id: u64,
}

/// Summary of a successful `rmdir`.
#[derive(Debug, Clone)]
pub struct RmdirResult {
    /// Catalog path that was removed.
    pub path: String,
    /// Object id of the removed directory marker.
    pub object_id: u64,
}

/// Summary of a successful `rename`. For directories `moved_entries` is
/// `1 + descendant_count`; for files it is always `1`.
#[derive(Debug, Clone)]
pub struct RenameResult {
    /// Source catalog path.
    pub old: String,
    /// Destination catalog path.
    pub new: String,
    /// Number of catalog entries that were rewritten (the entry itself
    /// plus every descendant when renaming a directory).
    pub moved_entries: usize,
}

/// Per-name outcome inside a [`Gateway::remove_objects_batch`] response.
/// Success carries the removed object's `object_id` for symmetry with
/// [`RemoveResult`]; every failure kind is a distinct `error` string so
/// clients can dispatch on it without parsing.
#[derive(Debug, Clone)]
pub struct BatchDeleteOutcome {
    pub name: String,
    /// `Some` iff the entry was actually removed. `None` on any failure
    /// (`error` populated).
    pub object_id: Option<u64>,
    /// One of: `"not_found"`, `"is_directory"`, `"bad_request: <detail>"`.
    /// Empty on success.
    pub error: String,
}

/// Aggregate result of [`Gateway::remove_objects_batch`]. `removed` +
/// `errors.len()` equals the request's `names.len()`.
#[derive(Debug, Clone, Default)]
pub struct BatchDeleteResult {
    /// Number of catalog entries actually removed.
    pub removed: usize,
    /// Per-name outcome, in request order.
    pub outcomes: Vec<BatchDeleteOutcome>,
    /// Number of shard hashes purged from the cluster (union across
    /// every successfully-removed manifest, minus hashes still
    /// referenced by other catalog entries or version archives).
    pub orphan_shards_purged: usize,
}

impl Gateway {
    /// Remove a data object by catalog name. Returns `NotFound` if the
    /// entry is missing, `IsDirectory` if the entry is a directory in the
    /// catalog, `Decode` if the cluster Purge partially fails. Directory
    /// entries cannot be deleted via this method — use [`Self::rmdir`].
    pub async fn remove_object(&self, name: &str) -> Result<RemoveResult, GatewayError> {
        catalog_path::validate(name)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.write().await;
        if matches!(cat.get(name), Some(m) if m.kind == ObjectKind::Directory) {
            return Err(GatewayError::IsDirectory);
        }
        let manifest = cat.remove(name).ok_or(GatewayError::NotFound)?;
        drop(cat);
        self.invalidate_cache(name).await;
        self.mark_catalog_dirty(name).await;
        self.persist_catalog().await?;
        let live = self.effective_live().await;
        // fix: purge ONLY the shards unique to `manifest`.
        // The legacy `purge_object(&manifest, &live)` deleted the
        // entire `(object_id, *, *)` bucket on each node, which broke
        // any other catalog entry that happened to share the same
        // `data_cid`. `manifest` was already removed from the catalog
        // above, so `purge_orphans_of` walks what remains and only
        // ships hashes nobody else still references to PurgeByHash.
        if let Err(e) = self.purge_orphans_of(&manifest, &live, None).await {
            return Err(GatewayError::Decode(format!("partial purge: {e}")));
        }
        Ok(RemoveResult {
            name: name.to_string(),
            object_id: manifest.object_id,
        })
    }

    /// Delete N objects under a single catalog write-lock + single
    /// `persist_catalog` fsync + single fanout PurgeByHash. Purpose:
    /// bulk cleanup (test drain, admin sweep). Per-name remove is
    /// still `remove_object` — the batch semantics are only about
    /// amortising the expensive cluster-wide steps.
    ///
    /// The `holofs-stability` 25 k-object drain used to take ~2.5 h
    /// via per-object DELETE; the batch path plus the v4 reverse
    /// index in `Directory::orphan_hashes` reduces that to seconds.
    ///
    /// Per-name failures (not-found, is-directory, invalid path) are
    /// reported in `outcomes` — the batch does not abort on one bad
    /// name. Any cluster-side error during the shared purge is
    /// surfaced through the return value (best-effort semantics: the
    /// catalog change already committed, so failure means shards
    /// leak until the next `/api/gc`).
    pub async fn remove_objects_batch(
        &self,
        names: &[String],
    ) -> Result<BatchDeleteResult, GatewayError> {
        use std::collections::HashSet;

        let mut result = BatchDeleteResult {
            removed: 0,
            outcomes: Vec::with_capacity(names.len()),
            orphan_shards_purged: 0,
        };
        // Collect the removed manifests so we can compute the union of
        // orphan candidates after we've released the write-lock.
        let mut removed_manifests: Vec<Manifest> = Vec::new();
        {
            let mut cat = self.catalog.write().await;
            for name in names {
                if let Err(e) = catalog_path::validate(name) {
                    result.outcomes.push(BatchDeleteOutcome {
                        name: name.clone(),
                        object_id: None,
                        error: format!("bad_request: {e}"),
                    });
                    continue;
                }
                match cat.get(name).map(|m| m.kind) {
                    None => {
                        result.outcomes.push(BatchDeleteOutcome {
                            name: name.clone(),
                            object_id: None,
                            error: "not_found".into(),
                        });
                    }
                    Some(ObjectKind::Directory) => {
                        result.outcomes.push(BatchDeleteOutcome {
                            name: name.clone(),
                            object_id: None,
                            error: "is_directory".into(),
                        });
                    }
                    Some(_) => {
                        let m = cat.remove(name).expect("checked present above");
                        result.outcomes.push(BatchDeleteOutcome {
                            name: name.clone(),
                            object_id: Some(m.object_id),
                            error: String::new(),
                        });
                        result.removed += 1;
                        removed_manifests.push(m);
                    }
                }
            }
        }
        // Invalidate cached decodes for every removed name. Cheap —
        // Mutex per name, no cluster I/O.
        for outcome in &result.outcomes {
            if outcome.error.is_empty() {
                self.invalidate_cache(&outcome.name).await;
            }
        }
        // Mark every removed name dirty before the batch persist so
        // the redb apply_batch drops the same set of keys in a single
        // write-txn.
        self.mark_catalog_dirty_many(
            result
                .outcomes
                .iter()
                .filter(|o| o.error.is_empty())
                .map(|o| o.name.clone()),
        )
        .await;
        // ONE catalog fsync for the entire batch. The persist ticket
        // coalesces callers so overlapping single-DELETEs would already
        // share an fsync, but the batch skips even the ticket dance.
        self.persist_catalog().await?;

        if removed_manifests.is_empty() {
            return Ok(result);
        }
        // Union every removed manifest's shard hashes and ask the
        // catalog's reverse index which are now orphans. Same
        // `exclude_name = None` semantics as `remove_object` (the
        // catalog no longer contains any of these entries).
        let mut candidate_hashes: HashSet<[u8; 32]> = HashSet::new();
        for m in &removed_manifests {
            for chan in &m.shard_hashes {
                for per_l in chan {
                    for h in per_l {
                        candidate_hashes.insert(*h);
                    }
                }
            }
        }
        if candidate_hashes.is_empty() {
            return Ok(result);
        }
        let live = self.effective_live().await;
        let orphans_from_catalog: Vec<[u8; 32]> = {
            let cat = self.catalog.read().await;
            cat.orphan_hashes(candidate_hashes.iter(), None)
        };
        if orphans_from_catalog.is_empty() {
            return Ok(result);
        }
        // Subtract hashes still referenced by version archives on disk.
        let mut orphans: HashSet<[u8; 32]> = orphans_from_catalog.into_iter().collect();
        let versions_dir = self
            .versions
            .lock()
            .await
            .root
            .clone()
            .map(|r| r.join("versions"));
        if let Some(dir) = versions_dir {
            if dir.exists() {
                if let Ok(by_name) = std::fs::read_dir(&dir) {
                    for name_entry in by_name.flatten() {
                        let path = name_entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        if let Ok(versions) = std::fs::read_dir(&path) {
                            for v_entry in versions.flatten() {
                                let p = v_entry.path();
                                if p.extension().and_then(|s| s.to_str()) != Some("bin") {
                                    continue;
                                }
                                let bytes = match std::fs::read(&p) {
                                    Ok(b) => b,
                                    Err(_) => continue,
                                };
                                let m = match Manifest::decode(&bytes) {
                                    Ok(m) => m,
                                    Err(_) => continue,
                                };
                                for chan in &m.shard_hashes {
                                    for per_l in chan {
                                        for h in per_l {
                                            orphans.remove(h);
                                            if orphans.is_empty() {
                                                return Ok(result);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if orphans.is_empty() {
            return Ok(result);
        }
        // ONE fanout PurgeByHash for every live node. The prior per-
        // object flow sent one RPC per node per DELETE — the batch
        // ships every orphan hash in one round trip per node.
        let orphan_vec: Vec<[u8; 32]> = orphans.into_iter().collect();
        for &node_idx in &live {
            let Some(addr) = self.cluster.node_addrs.get(node_idx).cloned() else {
                continue;
            };
            if let Err(e) = holofs_client::purge_node_by_hash(&addr, orphan_vec.clone()).await {
                eprintln!("remove_objects_batch: PurgeByHash on {addr}: {e}");
            }
        }
        result.orphan_shards_purged = orphan_vec.len();
        Ok(result)
    }

    /// Create a `Directory` entry at `path`. The parent (if any) must
    /// already exist as a directory; the path itself must not be taken.
    /// Returns `AlreadyExists` (target taken), `NotADirectory` (parent is
    /// not a directory), `BadRequest` (parent missing or path malformed).
    pub async fn mkdir(&self, path: &str) -> Result<MkdirResult, GatewayError> {
        catalog_path::validate(path)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.write().await;
        if cat.get(path).is_some() {
            return Err(GatewayError::AlreadyExists);
        }
        if let Some(parent) = catalog_path::parent(path) {
            match cat.get(parent) {
                Some(m) if m.kind == ObjectKind::Directory => {}
                Some(_) => return Err(GatewayError::NotADirectory),
                None => {
                    return Err(GatewayError::BadRequest(format!(
                        "parent directory does not exist: {parent}"
                    )))
                }
            }
        }
        let manifest = Manifest::directory(directory_object_id(path), now_unix());
        let object_id = manifest.object_id;
        cat.insert(path.to_string(), manifest);
        drop(cat);
        self.mark_catalog_dirty(path).await;
        self.persist_catalog().await?;
        Ok(MkdirResult {
            path: path.to_string(),
            object_id,
        })
    }

    /// Remove an empty directory. Errors:
    /// - `NotFound`: no entry at `path`.
    /// - `NotADirectory`: entry exists but is a data object.
    /// - `DirectoryNotEmpty`: at least one entry has `path` as a prefix.
    pub async fn rmdir(&self, path: &str) -> Result<RmdirResult, GatewayError> {
        catalog_path::validate(path)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        let mut cat = self.catalog.write().await;
        match cat.get(path) {
            Some(m) if m.kind == ObjectKind::Directory => {}
            Some(_) => return Err(GatewayError::NotADirectory),
            None => return Err(GatewayError::NotFound),
        }
        // BTreeMap range scan: anything that starts with `path + "/"` is a
        // descendant. The first such key is enough to refuse the call.
        let child_prefix = format!("{path}/");
        if cat
            .entries
            .range(child_prefix.clone()..)
            .next()
            .map(|(k, _)| k.starts_with(&child_prefix))
            .unwrap_or(false)
        {
            return Err(GatewayError::DirectoryNotEmpty);
        }
        let removed = cat.remove(path).expect("checked above");
        drop(cat);
        self.mark_catalog_dirty(path).await;
        self.persist_catalog().await?;
        Ok(RmdirResult {
            path: path.to_string(),
            object_id: removed.object_id,
        })
    }

    /// Rename an entry. For a directory all descendants are rewritten too;
    /// the rename is atomic w.r.t. the catalog mutex but **not** w.r.t. the
    /// on-disk catalog (a crash between mutation and persist could leave
    /// the old name visible after restart — same as PUT/DELETE today).
    ///
    /// Errors:
    /// - `NotFound`: no entry at `old`.
    /// - `AlreadyExists`: an entry already lives at `new` (or any
    ///   descendant target collides during a directory rename).
    /// - `BadRequest`: parent of `new` missing, or `new` would be a
    ///   descendant of `old` (cycle).
    pub async fn rename(&self, old: &str, new: &str) -> Result<RenameResult, GatewayError> {
        catalog_path::validate(old)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        catalog_path::validate(new)
            .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        if old == new {
            return Ok(RenameResult {
                old: old.to_string(),
                new: new.to_string(),
                moved_entries: 0,
            });
        }
        // Refuse to move a directory into itself.
        if new == old || new.starts_with(&format!("{old}/")) {
            return Err(GatewayError::BadRequest(
                "cannot rename a directory into its own descendant".into(),
            ));
        }
        let mut cat = self.catalog.write().await;
        // v3-6: Arc-clone the existing entry instead of deep-cloning
        // the whole Manifest just to re-wrap it. `insert_arc` on the
        // new key shares the same backing storage — mv on a large
        // manifest (with a full `shard_hashes` tree) used to
        // duplicate ~1 MB for no reason.
        let entry_arc = cat
            .entries
            .get(old)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if cat.get(new).is_some() {
            return Err(GatewayError::AlreadyExists);
        }
        if let Some(parent) = catalog_path::parent(new) {
            match cat.get(parent) {
                Some(m) if m.kind == ObjectKind::Directory => {}
                Some(_) => return Err(GatewayError::NotADirectory),
                None => {
                    return Err(GatewayError::BadRequest(format!(
                        "parent directory does not exist: {parent}"
                    )))
                }
            }
        }
        let mut moved: Vec<(String, String, std::sync::Arc<Manifest>)> = Vec::new();
        let entry_kind = entry_arc.kind;
        moved.push((old.to_string(), new.to_string(), entry_arc));
        if entry_kind == ObjectKind::Directory {
            let child_prefix = format!("{old}/");
            for (k, v) in cat.entries.range(child_prefix.clone()..) {
                if !k.starts_with(&child_prefix) {
                    break;
                }
                let suffix = &k[child_prefix.len()..];
                let target = format!("{new}/{suffix}");
                if cat.get(&target).is_some() {
                    return Err(GatewayError::AlreadyExists);
                }
                moved.push((k.clone(), target, std::sync::Arc::clone(v)));
            }
        }
        let count = moved.len();
        let mut dirty_names: Vec<String> = Vec::with_capacity(moved.len() * 2);
        for (from, to, manifest) in moved {
            cat.remove(&from);
            cat.insert_arc(to.clone(), manifest);
            dirty_names.push(from);
            dirty_names.push(to);
        }
        drop(cat);
        self.invalidate_cache(old).await;
        self.mark_catalog_dirty_many(dirty_names).await;
        self.persist_catalog().await?;
        Ok(RenameResult {
            old: old.to_string(),
            new: new.to_string(),
            moved_entries: count,
        })
    }

    /// Immediate children of `prefix`. Pass `""` for the root listing.
    /// Returned tuples are `(full_path, manifest)`; the basename is
    /// `full_path[prefix.len() + 1..]` for non-root prefixes.
    ///
    /// Errors `NotADirectory` if `prefix` is a real entry but not a
    /// directory; `NotFound` if `prefix` is non-empty and unknown.
    pub async fn list_dir(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, Manifest)>, GatewayError> {
        if !prefix.is_empty() {
            catalog_path::validate(prefix)
                .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
        }
        let cat = self.catalog.read().await;
        if !prefix.is_empty() {
            match cat.get(prefix) {
                Some(m) if m.kind == ObjectKind::Directory => {}
                Some(_) => return Err(GatewayError::NotADirectory),
                None => return Err(GatewayError::NotFound),
            }
        }
        let (start, child_prefix_len) = if prefix.is_empty() {
            (String::new(), 0usize)
        } else {
            let cp = format!("{prefix}/");
            let len = cp.len();
            (cp, len)
        };
        let mut out: Vec<(String, Manifest)> = Vec::new();
        for (k, v) in cat.entries.range(start.clone()..) {
            if !prefix.is_empty() && !k.starts_with(&start) {
                break;
            }
            if k == prefix {
                continue;
            }
            // Skip non-immediate descendants: the remainder after the
            // prefix must contain no '/'.
            let remainder = &k[child_prefix_len..];
            if remainder.is_empty() || remainder.contains('/') {
                continue;
            }
            out.push((k.clone(), (**v).clone()));
        }
        Ok(out)
    }

    /// Enumerate every non-directory catalog entry, flat, in
    /// BTreeMap order. Powers the `holofs-admin export-all` bulk
    /// backup path (P0.2) — the CLI walks this list once and issues
    /// one HTTP GET per entry.
    ///
    /// Directory markers are excluded because bulk-import re-derives
    /// them by re-PUTting objects whose names contain `/`. Returned
    /// tuple is `(name, kind, total_size_bytes)`:
    /// `total_size_bytes` is the sum of every declared shard length
    /// per channel/layer — an upper bound on the payload the
    /// consumer of `/<name>` will need to buffer, useful for the
    /// backup driver's progress bar.
    pub async fn list_all_objects(&self) -> Vec<CatalogEntry> {
        let cat = self.catalog.read().await;
        let mut out = Vec::with_capacity(cat.entries.len());
        for (name, manifest) in &cat.entries {
            if manifest.kind == ObjectKind::Directory {
                continue;
            }
            let size: u64 = manifest
                .sym_len
                .iter()
                .map(|s| *s as u64)
                .sum::<u64>()
                .saturating_mul(manifest.channels as u64);
            out.push(CatalogEntry {
                name: name.clone(),
                kind: manifest.kind,
                size,
            });
        }
        out
    }
}

/// One row of [`Gateway::list_all_objects`]. Meant to be
/// serialised into an admin bulk-listing response and consumed by
/// the `holofs-admin export-all` driver.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Full catalog path, e.g. `docs/2026/note.txt`.
    pub name: String,
    /// Object kind — image / audio / text / opaque.
    pub kind: ObjectKind,
    /// Upper-bound plaintext size in bytes (sum of `sym_len` × channels).
    pub size: u64,
}
