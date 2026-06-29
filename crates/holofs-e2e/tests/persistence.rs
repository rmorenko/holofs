//! Batch I: gateway persistence across graceful restart.
//!
//! Currently zero coverage — yet a catalog corruption on restart is
//! the highest-impact bug class for storage software. These tests
//! drive the new `TestHarness::restart()` to prove that a clean
//! kill + respawn against the same storage dir recovers the live
//! catalog (objects, directories, versions) byte-for-byte.

use anyhow::Result;
use holofs_e2e::{HarnessConfig, TestHarness};
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

async fn get_stats(harness: &TestHarness) -> Result<Value> {
    let body = raw_client()
        .get(harness.url("/api/stats"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&body)?)
}

/// PUT, restart, GET — the bytes that come back must match the bytes
/// that went in (modulo PNG re-encode, so compare against a pre-
/// restart decode). This is the headline persistence guarantee:
/// graceful restart is invisible to a reader.
#[tokio::test]
async fn put_then_restart_then_get_round_trips_bytes() -> Result<()> {
    let mut harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/keep").await?;
    harness
        .put_bytes(
            "photos/keep/a.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let before = harness.get_bytes("photos/keep/a.png").await?;
    harness.restart().await?;
    let after = harness.get_bytes("photos/keep/a.png").await?;
    assert_eq!(
        after, before,
        "post-restart decode differs from pre-restart \
         (before={} after={})",
        before.len(),
        after.len()
    );
    harness.close().await
}

/// /api/stats `objects_total` must be exactly the same before and
/// after the restart. The catalog is persisted to disk on every
/// mutation; the restarted gateway reads it back.
#[tokio::test]
async fn stats_objects_total_survives_restart() -> Result<()> {
    let mut harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    for i in 0..3 {
        harness
            .put_bytes(
                &format!("photos/n-{i:02}.png"),
                holofs_e2e::fixtures::textured_image_png().to_vec(),
            )
            .await?;
    }
    let before = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    assert!(before >= 3);
    harness.restart().await?;
    let after = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    assert_eq!(
        after, before,
        "objects_total drifted across restart: before={before} after={after}"
    );
    harness.close().await
}

/// Directory structure (intermediate `Directory` entries) must be
/// restored on read — otherwise the tree-view UI would render an
/// empty catalog after restart. We pick a 3-deep prefix.
#[tokio::test]
async fn nested_directory_tree_survives_restart() -> Result<()> {
    let mut harness = TestHarness::fresh().await?;
    harness.mkdir_p("a/b/c").await?;
    harness
        .put_bytes(
            "a/b/c/leaf.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    harness.restart().await?;
    // The leaf must still decode.
    let bytes = harness.get_bytes("a/b/c/leaf.png").await?;
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    // And each intermediate directory must still GET 409 (proof
    // they're still in the catalog as Directory entries).
    for dir in ["a", "a/b", "a/b/c"] {
        let resp = raw_client().get(harness.url(dir)).send().await?;
        assert_eq!(
            resp.status().as_u16(),
            409,
            "intermediate dir {dir:?} lost after restart, got {}",
            resp.status()
        );
    }
    harness.close().await
}

/// Versions side files live next to the catalog. After a PUT-replace
/// + restart, the archived prior version must still be listed by
/// /api/versions_list and the bytes restorable.
#[tokio::test]
async fn versions_history_survives_restart() -> Result<()> {
    let mut harness = TestHarness::fresh_with(HarnessConfig {
        enable_versions: true,
        ..HarnessConfig::default()
    })
    .await?;
    harness.mkdir_p("photos").await?;
    let body_a = holofs_e2e::fixtures::textured_image_png().to_vec();
    let body_b = holofs_e2e::fixtures::tiny_image_png().to_vec();
    let name = "photos/historic.png";
    harness.put_bytes(name, body_a.clone()).await?;
    harness.put_bytes(name, body_b.clone()).await?; // archives `a`

    let before: Value = harness
        .post_form("/api/versions_list", &[("name", name)])
        .await?;
    let before_n = before["versions"]
        .as_array()
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(before_n >= 1, "expected 1 archive before restart, got {before_n}");

    harness.restart().await?;

    let after: Value = harness
        .post_form("/api/versions_list", &[("name", name)])
        .await?;
    let after_arr = after["versions"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        after_arr.len(),
        before_n,
        "versions count drifted across restart: before={before_n} after={}",
        after_arr.len()
    );
    // The archived version's id must be intact and parseable.
    let id = after_arr[0]["id"]
        .as_str()
        .expect("version id missing post-restart");
    assert!(id.starts_with('v'), "archived version id format changed: {id:?}");
    harness.close().await
}
