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
        let entry = cat.get(old).cloned().ok_or(GatewayError::NotFound)?;
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
        moved.push((old.to_string(), new.to_string(), std::sync::Arc::new(entry.clone())));
        if entry.kind == ObjectKind::Directory {
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
        for (from, to, manifest) in moved {
            cat.remove(&from);
            cat.insert_arc(to, manifest);
        }
        drop(cat);
        self.invalidate_cache(old).await;
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
}
