//! `#[server]` functions consumed by every catalog-view component.
//!
//! Three functions live here:
//!
//! - [`get_catalog`] — the historical eager path. Returns every catalog
//!   entry that matches the URL-level filter, plus ancestor directories
//!   of any surviving leaf so tree paths stay navigable. Used by
//!   [`crate::CatalogTreeEager`] when a filter is active.
//! - [`list_dir`] — one-level children under a prefix. Used by the
//!   focus-view page + as a fallback under filters.
//! - [`list_dir_page`] — paginated one-level children. Powers the
//!   Stage 11.21 / 11.22 lazy tree.
//!
//! [`TreeSort`] is the shared "which column drives ordering" enum and
//! [`compare_entries`] is the shared comparator so the eager tree
//! ordering (`build_tree`) and the paged tree ordering
//! (`list_dir_page`) stay in lock-step.
//!
//! Moved out of `lib.rs` in Phase R2a.1.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::catalog_types::CatalogEntry;

/// Catalog snapshot for `GET /`. Reads the live `Gateway` from leptos
/// context; under hydrate the macro emits a client stub that POSTs to
/// `/api/get_catalog`. `endpoint = "get_catalog"` pins the URL — without
/// it the macro appends a hash to the path.
#[server(
    name = GetCatalog,
    prefix = "/api",
    endpoint = "get_catalog",
)]
pub async fn get_catalog(
    name_glob: String,
    date_from: String,
    date_to: String,
) -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    use crate::filter::{apply_filter, CatalogFilter};

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let catalog = gw.catalog().lock().await;
    let filter = CatalogFilter::parse(&name_glob, &date_from, &date_to)
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e))?;
    let raw: Vec<CatalogEntry> = catalog
        .entries
        .iter()
        .map(|(name, m)| CatalogEntry::from_manifest(name, m))
        .collect();
    // Stage 11.17: apply server-side filter. Filter is matched against
    // leaves (non-directory entries); ancestor directories of any
    // matching leaf are kept automatically so the tree still has paths
    // to render. Directories whose own name/date matches the filter
    // also stay (so users can `q=Roman*` and find that folder
    // directly).
    let mut out: Vec<CatalogEntry> = if filter.is_empty() {
        raw
    } else {
        apply_filter(raw, &filter)
    };
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `GET /api/list_dir?prefix=...` — children one level under `prefix`.
/// Pass an empty string for the root listing. Errors map 1:1 to
/// `holofs_gateway::GatewayError`: 404 if the prefix is unknown, 409 if
/// it's a real object, 400 if it's malformed.
#[server(
    name = ListDir,
    prefix = "/api",
    endpoint = "list_dir",
)]
pub async fn list_dir(
    prefix: String,
    name_glob: String,
    date_from: String,
    date_to: String,
) -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    use crate::filter::CatalogFilter;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let children = gw
        .list_dir(&prefix)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let filter = CatalogFilter::parse(&name_glob, &date_from, &date_to)
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e))?;
    let mut out: Vec<CatalogEntry> = children
        .into_iter()
        .map(|(name, m)| CatalogEntry::from_manifest(&name, &m))
        .filter(|e| filter.is_empty() || filter.matches(e))
        .collect();
    // Directories first, then objects — both alphabetical inside the bucket.
    out.sort_by(|a, b| {
        let a_dir = a.kind == "directory";
        let b_dir = b.kind == "directory";
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Stage 11.21 / 11.22: a single page of a directory listing. `entries`
/// is `[offset..offset+limit]` slice of the children, sorted with the
/// same key the rest of the tree view uses (dirs first, then by
/// `TreeSort`). `has_more` is `true` when there are more entries past
/// the slice; the client uses it to decide whether to render a
/// load-more sentinel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ListDirPage {
    /// This page's children (already sorted).
    pub entries: Vec<CatalogEntry>,
    /// `true` when the directory has more entries past this page.
    pub has_more: bool,
    /// Total number of children in the directory (across all pages),
    /// for "1234 entries" counters in the UI.
    pub total: u32,
}

/// `GET /api/list_dir_page?prefix=&offset=&limit=&sort=` — paginated
/// children for the lazy tree view (Stage 11.21 / 11.22). Distinct
/// from [`list_dir`] which returns the whole directory at once for the
/// focus view — when filters are active the lazy tree falls back to
/// the eager `get_catalog` path so prefix-less pagination would
/// duplicate work.
#[server(
    name = ListDirPageFn,
    prefix = "/api",
    endpoint = "list_dir_page",
)]
pub async fn list_dir_page(
    prefix: String,
    offset: u32,
    limit: u32,
    sort: String,
) -> Result<ListDirPage, ServerFnError> {
    use std::sync::Arc;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let children = gw
        .list_dir(&prefix)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let mut all: Vec<CatalogEntry> = children
        .into_iter()
        .map(|(name, m)| CatalogEntry::from_manifest(&name, &m))
        .collect();
    // Directories always group first; inside each group, apply the
    // user-selected sort (matches the eager tree's behaviour).
    let sort_key = TreeSort::from_param(&sort);
    all.sort_by(|a, b| {
        let a_dir = a.kind == "directory";
        let b_dir = b.kind == "directory";
        b_dir
            .cmp(&a_dir)
            .then_with(|| compare_entries(a, b, sort_key))
    });
    let total = all.len() as u32;
    let off = offset.min(total) as usize;
    let lim = limit.max(1) as usize;
    let slice: Vec<CatalogEntry> = all.into_iter().skip(off).take(lim).collect();
    let has_more = (off + slice.len()) < total as usize;
    Ok(ListDirPage {
        entries: slice,
        has_more,
        total,
    })
}

/// Tree column ordering signal. Threaded through every tree
/// component and read from the `?sort=` query param.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeSort {
    Name,
    Size,
    Kind,
    Date,
}

impl TreeSort {
    pub(crate) fn from_param(s: &str) -> Self {
        match s {
            "size" => Self::Size,
            "kind" => Self::Kind,
            "date" => Self::Date,
            _ => Self::Name,
        }
    }
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Kind => "kind",
            Self::Date => "date",
        }
    }
}

/// Comparator for catalog entries within a (directory or file) group.
/// Centralised so [`list_dir_page`] and the eager `build_tree` use the
/// same orderings for each `TreeSort`.
#[cfg(feature = "ssr")]
pub(crate) fn compare_entries(
    a: &CatalogEntry,
    b: &CatalogEntry,
    sort: TreeSort,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match sort {
        TreeSort::Name => a.name.cmp(&b.name),
        TreeSort::Size => b.n_shards.cmp(&a.n_shards).then(a.name.cmp(&b.name)),
        TreeSort::Kind => a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)),
        TreeSort::Date => {
            // Newest first; zero (legacy / unknown) sinks to the bottom.
            let ord = match (a.created_at_unix, b.created_at_unix) {
                (0, 0) => Ordering::Equal,
                (0, _) => Ordering::Greater,
                (_, 0) => Ordering::Less,
                (x, y) => y.cmp(&x),
            };
            ord.then(a.name.cmp(&b.name))
        }
    }
}
