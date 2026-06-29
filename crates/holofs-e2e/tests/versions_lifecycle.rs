//! /versions lifecycle: archive on PUT-replace, list, delete, retention cap.
//!
//! All tests hit the gateway HTTP API directly (no WebDriver) because
//! the underlying state can be observed via `/api/versions_list` and
//! `POST /api/versions/delete`. UI rendering is covered separately in
//! the `ui_versions.rs` suite.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use serde_json::Value;

/// Helper: pull the version array for `name` via the SSR endpoint.
/// `/api/versions_list` is a leptos server function — form-encoded
/// POST body, JSON response.
async fn list_versions(harness: &TestHarness, name: &str) -> Result<Vec<Value>> {
    let v: Value = harness
        .post_form("/api/versions_list", &[("name", name)])
        .await?;
    let arr = v
        .get("versions")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr)
}

/// PUT, PUT-replace, then delete the archived version. Listing the
/// versions before and after must change accordingly.
#[tokio::test]
async fn delete_version_removes_archive_and_drops_listing() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body_a = holofs_e2e::fixtures::textured_image_png().to_vec();
    let body_b = holofs_e2e::fixtures::tiny_image_png().to_vec();
    let name = "photos/abstract/lifecycle.png";

    harness.put_bytes(name, body_a.clone()).await?;
    harness.put_bytes(name, body_b.clone()).await?; // archives `a`
    let before = list_versions(&harness, name).await?;
    assert_eq!(before.len(), 1, "expected one archived version, got {}", before.len());
    let id = before[0]
        .get("id")
        .and_then(|x| x.as_str())
        .expect("version entry missing 'id'")
        .to_string();

    // POST the form. Use a no-redirect client so we observe the 303
    // directly instead of being bounced to the /versions HTML page.
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect
        .post(harness.url("/api/versions/delete"))
        .form(&[("name", name), ("id", id.as_str()), ("return_to", "/")])
        .send()
        .await?;
    assert_eq!(
        resp.status().as_u16(),
        303,
        "expected 303 redirect from /api/versions/delete, got {}",
        resp.status()
    );

    let after = list_versions(&harness, name).await?;
    assert!(
        after.is_empty(),
        "expected zero versions after delete, got {}",
        after.len()
    );

    // The current catalog entry must still decode — deleting an
    // archived version should never disturb the live object.
    let bytes = harness.get_bytes(name).await?;
    assert!(
        bytes.len() > 100,
        "live object decode after version delete returned {} bytes",
        bytes.len()
    );

    harness.close().await
}

/// Deleting a non-existent version id returns 404, not 500. Guards the
/// handler's error mapping.
#[tokio::test]
async fn delete_version_on_unknown_id_returns_404() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let name = "photos/abstract/no-such-version.png";
    harness.put_bytes(name, body.clone()).await?;

    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect
        .post(harness.url("/api/versions/delete"))
        .form(&[
            ("name", name),
            ("id", "v9999999999_deadbeef"),
            ("return_to", "/"),
        ])
        .send()
        .await?;
    assert_eq!(
        resp.status().as_u16(),
        404,
        "expected 404 for unknown version id, got {}",
        resp.status()
    );

    harness.close().await
}

/// Deleting a version must not break the live catalog entry — even
/// if both shared identical bytes (dedup'd `data_cid`). The orphan-
/// purge inside delete_version must skip hashes the live entry still
/// owns.
#[tokio::test]
async fn delete_version_does_not_break_dedup_sibling() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let other = holofs_e2e::fixtures::tiny_image_png().to_vec();
    let name = "photos/abstract/dedup-sibling.png";
    let twin = "photos/abstract/dedup-twin.png";

    // Two distinct names with identical bytes — same data_cid, same
    // object_id, shared shard buckets.
    harness.put_bytes(name, body.clone()).await?;
    harness.put_bytes(twin, body.clone()).await?;
    // Snapshot `twin`'s decoded bytes BEFORE the version-delete
    // operation. PNG re-encoding isn't byte-identical to the input,
    // so we compare round-tripped decode == decode, not == body.
    let twin_before = harness.get_bytes(twin).await?;

    // PUT-replace `name` so the original (which still shares shards
    // with `twin`) becomes an archived version of `name`.
    harness.put_bytes(name, other.clone()).await?;
    let versions = list_versions(&harness, name).await?;
    assert_eq!(versions.len(), 1);
    let id = versions[0]["id"].as_str().unwrap().to_string();

    // Delete that archived version. Its shard hashes overlap with
    // `twin`'s — the purge MUST honour the catalog reference.
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect
        .post(harness.url("/api/versions/delete"))
        .form(&[("name", name), ("id", id.as_str()), ("return_to", "/")])
        .send()
        .await?;
    assert_eq!(resp.status().as_u16(), 303);

    // `twin` must decode to the same bytes it did before the delete.
    let twin_after = harness.get_bytes(twin).await?;
    assert_eq!(
        twin_after, twin_before,
        "dedup sibling bytes changed after version delete \
         — orphan purge over-purged shared shards \
         (twin_before={} bytes, twin_after={} bytes)",
        twin_before.len(),
        twin_after.len()
    );

    harness.close().await
}

/// `HOLOFS_VERSIONS_KEEP_LAST=N` trims each name's archive to N most
/// recent on the next PUT. Smoke test: with cap = 2, four PUT-replaces
/// yield at most 2 archived versions.
#[tokio::test]
async fn retention_cap_prunes_oldest_versions() -> Result<()> {
    let mut cfg = holofs_e2e::HarnessConfig::default();
    cfg.extra_env.push(("HOLOFS_VERSIONS_KEEP_LAST".into(), "2".into()));
    let harness = TestHarness::fresh_with(cfg).await?;
    harness.mkdir_p("photos/abstract").await?;
    let name = "photos/abstract/retention.png";

    let fixtures = [
        holofs_e2e::fixtures::textured_image_png(),
        holofs_e2e::fixtures::tiny_image_png(),
        holofs_e2e::fixtures::textured_image_png(),
        holofs_e2e::fixtures::tiny_image_png(),
    ];
    for (i, body) in fixtures.iter().enumerate() {
        // Tiny gap so the per-version filename timestamps don't collide.
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        harness.put_bytes(name, body.to_vec()).await?;
    }
    let versions = list_versions(&harness, name).await?;
    assert!(
        versions.len() <= 2,
        "retention cap=2 left {} versions on disk",
        versions.len()
    );

    harness.close().await
}
