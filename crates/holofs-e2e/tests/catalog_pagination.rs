//! P2.1 — /admin/catalog_names pagination end-to-end.
//!
//! Covers three shapes the endpoint has to support in one release:
//!
//! 1. Backward-compat bare-array response when no pagination params
//!    are passed (older `holofs-admin` binaries + curl scripts).
//! 2. Wrapped `{items, next_cursor}` response when `?cursor=` or
//!    `?limit=` is present.
//! 3. Cursor semantics: iterating until `next_cursor==null` returns
//!    exactly the union of the seeded entries.
//!
//! Runs with `--test-threads=1` per the workspace convention
//! (see `feedback_e2e_threads.md`).

use std::collections::HashSet;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn catalog_names_bare_array_when_no_params() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;

    // No query params → old bare-array shape. Must NOT be an object.
    let v = harness.get_json("/admin/catalog_names").await?;
    assert!(
        v.is_array(),
        "no-params response must remain a bare JSON array (backward compat); got: {v:?}"
    );
    let arr = v.as_array().unwrap();
    assert!(
        !arr.is_empty(),
        "seed_minimal seeded 2 objects, expected non-empty listing"
    );
    for row in arr {
        for f in ["name", "kind", "size"] {
            assert!(row.get(f).is_some(), "listing row missing `{f}`: {row}");
        }
    }
    harness.close().await
}

#[tokio::test]
async fn catalog_names_paginated_wraps_response_when_limit_is_set() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;

    // With `?limit=`, response is a wrapper — schema regression check
    // so any drift breaks CI, not silent-truncates a downstream tool.
    let v = harness.get_json("/admin/catalog_names?limit=1000").await?;
    assert!(
        v.is_object(),
        "paginated response must be an object with `items` + `next_cursor`; got: {v:?}"
    );
    let items = v
        .get("items")
        .and_then(|v| v.as_array())
        .expect("paginated response must have `items` array");
    assert!(!items.is_empty(), "seed_minimal → non-empty items");
    // `next_cursor` field is always present (either a String or null).
    assert!(
        v.get("next_cursor").is_some(),
        "paginated response must include `next_cursor` (may be null): {v:?}"
    );
    harness.close().await
}

#[tokio::test]
async fn catalog_names_paginated_walk_matches_full_listing() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;

    // Full listing via the bare-array path.
    let full = harness.get_json("/admin/catalog_names").await?;
    let full_names: HashSet<String> = full
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        !full_names.is_empty(),
        "seed_minimal should have produced at least one non-directory entry"
    );

    // Paginated walk with a tiny limit → must hit multiple pages.
    // The seed_minimal fixture produces only ~2 objects, so limit=1
    // guarantees at least two round trips.
    let mut collected: HashSet<String> = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut round_trips = 0;
    loop {
        let mut path = "/admin/catalog_names?limit=1".to_string();
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                path.push_str("&cursor=");
                path.push_str(c);
            }
        }
        let page = harness.get_json(&path).await?;
        round_trips += 1;
        assert!(
            round_trips < 100,
            "pagination loop runaway — 100 round trips for a seed_minimal catalog"
        );
        let items = page
            .get("items")
            .and_then(|v| v.as_array())
            .expect("paginated wrapper must carry `items`");
        for row in items {
            if let Some(name) = row.get("name").and_then(|v| v.as_str()) {
                collected.insert(name.to_string());
            }
        }
        match page.get("next_cursor") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => {
                cursor = Some(s.clone());
            }
            _ => break,
        }
    }
    assert_eq!(
        collected, full_names,
        "paginated walk must yield the same name set as the bare-array listing"
    );
    harness.close().await
}
