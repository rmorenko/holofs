//! Stage 9 integration tests: end-to-end mkdir / list_dir / rename / rmdir
//! against a real [`Gateway`]. The cluster machinery (live nodes,
//! `ingest_bytes`, `decode_object`) is **not** exercised here — these tests
//! intentionally use a zero-node Gateway because every directory operation
//! is a pure catalog mutation. Coverage of the data-plane lives under the
//! ingest smoke tests in `crates/holofs-web`.

use std::sync::Arc;

use tokio::sync::Mutex;

use holofs_client::LiveNodes;
use holofs_core::gf::Gf;
use holofs_gateway::{ClusterInfo, Gateway, GatewayError};
use holofs_model::fs::Directory;
use holofs_model::manifest::ObjectKind;
use holofs_model::placement::Placement;

fn build_gateway() -> Arc<Gateway> {
    let gf = Arc::new(Gf::new());
    let catalog = Arc::new(Mutex::new(Directory::new()));
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
        let mut cat = gw.catalog().lock().await;
        cat.insert(
            "docs/readme.txt".into(),
            holofs_model::manifest::Manifest::directory(0xAAAA),
        );
        cat.entries.get_mut("docs/readme.txt").unwrap().kind =
            holofs_model::manifest::ObjectKind::Text;
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

    let cat = gw.catalog().lock().await;
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
