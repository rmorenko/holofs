//! integration tests: end-to-end mkdir / list_dir / rename / rmdir
//! against a real [`Gateway`]. The cluster machinery (live nodes,
//! `ingest_bytes`, `decode_object`) is **not** exercised here — these tests
//! intentionally use a zero-node Gateway because every directory operation
//! is a pure catalog mutation. Coverage of the data-plane lives under the
//! ingest smoke tests in `crates/holofs-web`.

use std::sync::Arc;

use tokio::sync::RwLock;

use holofs_client::LiveNodes;
use holofs_core::gf::Gf;
use holofs_gateway::{ClusterInfo, Gateway, GatewayError};
use holofs_model::fs::Directory;
use holofs_model::manifest::ObjectKind;
use holofs_model::placement::Placement;

fn build_gateway() -> Arc<Gateway> {
    let gf = Arc::new(Gf::new());
    let catalog = Arc::new(RwLock::new(Directory::new()));
    let live: Arc<LiveNodes> = Arc::new(Vec::new());
    let cluster = Arc::new(ClusterInfo {
        node_addrs: Vec::new(),
        zones: Vec::new(),
        placement: Placement::Rendezvous,
        width: 0,
        height: 0,
    });
    Gateway::new(gf, catalog, live, cluster)
}

#[tokio::test]
async fn mkdir_then_list_dir() {
    let gw = build_gateway();
    gw.mkdir("photos").await.expect("mkdir photos");
    gw.mkdir("photos/2026").await.expect("mkdir photos/2026");
    gw.mkdir("docs").await.expect("mkdir docs");

    let root = gw.list_dir("").await.expect("list root");
    let names: Vec<&str> = root.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"photos"));
    assert!(names.contains(&"docs"));
    assert!(!names.contains(&"photos/2026")); // nested, not immediate

    let sub = gw.list_dir("photos").await.expect("list photos");
    let names: Vec<&str> = sub.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["photos/2026"]);

    // Every returned entry under our directory-only setup is itself a dir.
    for (_, m) in &sub {
        assert_eq!(m.kind, ObjectKind::Directory);
    }
}

#[tokio::test]
async fn mkdir_refuses_when_parent_missing() {
    let gw = build_gateway();
    let err = gw.mkdir("a/b/c").await.expect_err("parent missing");
    assert!(matches!(err, GatewayError::BadRequest(_)));
}

#[tokio::test]
async fn mkdir_refuses_when_target_exists() {
    let gw = build_gateway();
    gw.mkdir("dup").await.expect("first mkdir");
    let err = gw.mkdir("dup").await.expect_err("dup");
    assert!(matches!(err, GatewayError::AlreadyExists));
}

#[tokio::test]
async fn rmdir_requires_empty_directory() {
    let gw = build_gateway();
    gw.mkdir("photos").await.expect("mkdir photos");
    gw.mkdir("photos/2026").await.expect("mkdir nested");

    let err = gw.rmdir("photos").await.expect_err("photos has child");
    assert!(matches!(err, GatewayError::DirectoryNotEmpty));

    gw.rmdir("photos/2026").await.expect("inner rmdir");
    gw.rmdir("photos").await.expect("now empty");

    let root = gw.list_dir("").await.expect("list root");
    assert!(root.is_empty());
}

#[tokio::test]
async fn rmdir_rejects_non_directory() {
    let gw = build_gateway();
    gw.mkdir("docs").await.expect("mkdir docs");
    // Insert a fake non-directory entry by reaching into the catalog mutex.
    // We bypass ingest_bytes intentionally — this is a focused catalog test.
    {
        let mut cat = gw.catalog().write().await;
        cat.insert(
            "docs/readme.txt".into(),
            holofs_model::manifest::Manifest::directory(0xAAAA, 0),
        );
        cat.get_mut("docs/readme.txt").unwrap().kind = holofs_model::manifest::ObjectKind::Text;
    }
    let err = gw
        .rmdir("docs/readme.txt")
        .await
        .expect_err("not a directory");
    assert!(matches!(err, GatewayError::NotADirectory));
}

#[tokio::test]
async fn rename_carries_descendants_along() {
    let gw = build_gateway();
    gw.mkdir("a").await.unwrap();
    gw.mkdir("a/b").await.unwrap();
    gw.mkdir("a/b/c").await.unwrap();
    gw.mkdir("dest").await.unwrap();

    let res = gw.rename("a/b", "dest/b").await.expect("rename");
    assert_eq!(res.moved_entries, 2); // a/b + a/b/c

    let cat = gw.catalog().read().await;
    assert!(cat.get("a").is_some());
    assert!(cat.get("a/b").is_none());
    assert!(cat.get("a/b/c").is_none());
    assert!(cat.get("dest/b").is_some());
    assert!(cat.get("dest/b/c").is_some());
}

#[tokio::test]
async fn rename_refuses_cycle() {
    let gw = build_gateway();
    gw.mkdir("outer").await.unwrap();
    gw.mkdir("outer/inner").await.unwrap();
    let err = gw
        .rename("outer", "outer/inner/x")
        .await
        .expect_err("cycle");
    assert!(matches!(err, GatewayError::BadRequest(_)));
}

#[tokio::test]
async fn list_dir_on_unknown_prefix_is_not_found() {
    let gw = build_gateway();
    let err = gw.list_dir("nope").await.expect_err("unknown");
    assert!(matches!(err, GatewayError::NotFound));
}

// --- P2.1 catalog pagination ---

/// Seed a fixed set of non-directory catalog entries in BTreeMap order.
/// Uses the same reach-into-the-mutex trick as `rmdir_rejects_non_directory`
/// so we don't need to run the full ingest pipeline for a pure catalog
/// enumeration test.
async fn seed_objects(gw: &Gateway, names: &[&str]) {
    let mut cat = gw.catalog().write().await;
    for name in names {
        cat.insert(
            (*name).into(),
            holofs_model::manifest::Manifest::directory(0xDEAD, 0),
        );
        cat.get_mut(name).unwrap().kind = holofs_model::manifest::ObjectKind::Text;
    }
}

#[tokio::test]
async fn pagination_walks_full_catalog_in_batches() {
    let gw = build_gateway();
    // 5 non-directory entries + 2 real directories. The directory
    // manifests must be skipped by the enumerator but still count for
    // the range scan (i.e. must not short-circuit the cursor forward).
    gw.mkdir("photos").await.unwrap();
    gw.mkdir("docs").await.unwrap();
    seed_objects(&gw, &["a", "b", "c", "d", "e"]).await;

    let mut collected: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let limit = 2usize;
    loop {
        let page = gw
            .list_all_objects_paginated(cursor.as_deref(), limit)
            .await;
        for item in &page.items {
            collected.push(item.name.clone());
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        // Safety valve: catalog has 7 entries, at most 4 pages of 2.
        assert!(collected.len() <= 20, "runaway pagination loop");
    }
    assert_eq!(
        collected,
        vec!["a", "b", "c", "d", "e"],
        "pagination must yield every non-directory entry in BTreeMap order"
    );
}

#[tokio::test]
async fn pagination_next_cursor_is_none_on_last_page() {
    let gw = build_gateway();
    seed_objects(&gw, &["only_one"]).await;
    let page = gw.list_all_objects_paginated(None, 1000).await;
    assert_eq!(page.items.len(), 1);
    assert!(
        page.next_cursor.is_none(),
        "single-page walk must not hand back a next_cursor"
    );
}

#[tokio::test]
async fn pagination_empty_catalog_returns_empty_no_cursor() {
    let gw = build_gateway();
    let page = gw.list_all_objects_paginated(None, 100).await;
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn pagination_cursor_past_end_returns_empty() {
    let gw = build_gateway();
    seed_objects(&gw, &["alpha", "beta"]).await;
    // Cursor after the last key (`"zzz" > "beta"`) — no entries left.
    let page = gw.list_all_objects_paginated(Some("zzz"), 100).await;
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn pagination_skips_directories_transparently_in_one_page() {
    // Directories are skipped "for free" — a page whose limit could
    // theoretically span some directories AND some objects returns
    // *only* the objects, and if the whole range fits under `limit`
    // emitted entries, `next_cursor` is `None`.
    //
    // Layout in BTreeMap order:
    //   "a_dir" (dir), "b_dir" (dir), "c_obj" (text)
    // With limit=2, the walk emits "c_obj" only (1 < 2 → walk not
    // saturated → cursor exhausted → next_cursor=None).
    let gw = build_gateway();
    gw.mkdir("a_dir").await.unwrap();
    gw.mkdir("b_dir").await.unwrap();
    seed_objects(&gw, &["c_obj"]).await;

    let page = gw.list_all_objects_paginated(None, 2).await;
    let got: Vec<&str> = page.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(got, vec!["c_obj"], "directory entries must be filtered out");
    assert!(
        page.next_cursor.is_none(),
        "unsaturated walk (1 emitted < 2 limit) means the catalog is fully covered"
    );
}

#[tokio::test]
async fn pagination_next_cursor_set_when_limit_is_reached() {
    // A saturated page (emitted == limit) MUST hand back a cursor —
    // even if the actual last-visited entry is the very last object
    // in the catalog. That extra one-round-trip cost is the price of
    // "cursor==None means truly done" (see docstring). The caller's
    // next call returns an empty page with next_cursor=None.
    let gw = build_gateway();
    seed_objects(&gw, &["x", "y"]).await;

    let page1 = gw.list_all_objects_paginated(None, 2).await;
    let got: Vec<&str> = page1.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(got, vec!["x", "y"]);
    assert_eq!(page1.next_cursor.as_deref(), Some("y"));

    let page2 = gw
        .list_all_objects_paginated(page1.next_cursor.as_deref(), 2)
        .await;
    assert!(page2.items.is_empty());
    assert!(page2.next_cursor.is_none());
}

// --- P2.2 per-object retention ---

use holofs_model::manifest::RetentionPolicy;

#[tokio::test]
async fn retention_set_get_roundtrip() {
    let gw = build_gateway();
    seed_objects(&gw, &["foo"]).await;

    // Fresh objects have no retention.
    let p = gw.get_retention("foo").await.unwrap();
    assert_eq!(p, None);

    // Set a policy.
    gw.set_retention(
        "foo",
        Some(RetentionPolicy::ExpiresAt {
            expires_at_unix: 1_700_000_000,
        }),
    )
    .await
    .expect("set_retention");
    let p = gw.get_retention("foo").await.unwrap();
    assert_eq!(
        p,
        Some(RetentionPolicy::ExpiresAt {
            expires_at_unix: 1_700_000_000
        })
    );

    // Clear it.
    gw.set_retention("foo", None).await.expect("clear");
    let p = gw.get_retention("foo").await.unwrap();
    assert_eq!(p, None);
}

#[tokio::test]
async fn retention_get_refuses_directory() {
    let gw = build_gateway();
    gw.mkdir("stuff").await.unwrap();
    let err = gw.get_retention("stuff").await.expect_err("dir");
    assert!(matches!(err, GatewayError::IsDirectory));
}

#[tokio::test]
async fn retention_set_refuses_unknown_name() {
    let gw = build_gateway();
    let err = gw
        .set_retention(
            "nope",
            Some(RetentionPolicy::ExpiresAt {
                expires_at_unix: 100,
            }),
        )
        .await
        .expect_err("unknown");
    assert!(matches!(err, GatewayError::NotFound));
}

#[tokio::test]
async fn gc_tick_deletes_only_expired_objects() {
    let gw = build_gateway();
    seed_objects(&gw, &["fresh", "old"]).await;
    // "fresh" expires in the future, "old" in the past.
    gw.set_retention(
        "fresh",
        Some(RetentionPolicy::ExpiresAt {
            expires_at_unix: 2_000_000_000,
        }),
    )
    .await
    .unwrap();
    gw.set_retention(
        "old",
        Some(RetentionPolicy::ExpiresAt {
            expires_at_unix: 1_000,
        }),
    )
    .await
    .unwrap();

    // Tick at "now = 1_700_000_000" — "old" should trip, "fresh"
    // should NOT.
    let report = gw.gc_expired_objects_tick(1_700_000_000).await;
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].name, "old");
    assert!(report.outcomes[0].deleted);
    assert!(!report.hit_per_tick_cap);

    // Catalog: "fresh" survives, "old" gone.
    let names: Vec<String> = {
        let cat = gw.catalog().read().await;
        cat.entries.keys().cloned().collect()
    };
    assert!(names.contains(&"fresh".to_string()));
    assert!(!names.contains(&"old".to_string()));
}

#[tokio::test]
async fn gc_tick_skips_objects_without_retention() {
    let gw = build_gateway();
    seed_objects(&gw, &["a", "b"]).await;
    // Neither has retention → no deletions no matter how far in
    // the future the tick's `now_unix` is.
    let report = gw.gc_expired_objects_tick(u64::MAX).await;
    assert!(
        report.outcomes.is_empty(),
        "objects without retention must be immune to GC"
    );
    let names: Vec<String> = {
        let cat = gw.catalog().read().await;
        cat.entries.keys().cloned().collect()
    };
    assert!(names.contains(&"a".to_string()));
    assert!(names.contains(&"b".to_string()));
}

#[tokio::test]
async fn gc_tick_skips_directories() {
    // Directory manifests have no retention path anyway — the
    // enumerator filters them. Sanity-check that a directory doesn't
    // accidentally trip the sweep even if we tried to abuse it.
    let gw = build_gateway();
    gw.mkdir("things").await.unwrap();
    let report = gw.gc_expired_objects_tick(u64::MAX).await;
    assert!(report.outcomes.is_empty());
    let cat = gw.catalog().read().await;
    assert!(cat.get("things").is_some());
}
