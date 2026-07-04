//! `/versions/<name>` per-object history + `/api/restore`.
//!
//! End-to-end: PUT a file, PUT-replace it with different content,
//! confirm the version archive grew, restore via the JSON API, and
//! cross-check that the live bytes match the original.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn put_replace_archives_the_old_manifest() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    harness
        .put_bytes(
            "photos/abstract/v.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    // Initially no archived versions.
    let before = harness
        .post_form(
            "/api/versions_list",
            &[("name", "photos/abstract/v.png")],
        )
        .await?;
    let n_before = before
        .get("versions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(n_before, 0, "expected zero archived versions on a fresh PUT; got: {before}");

    // PUT-replace with a different fixture → archive grows by one.
    harness
        .put_bytes(
            "photos/abstract/v.png",
            holofs_e2e::fixtures::tiny_image_png().to_vec(),
        )
        .await?;
    let after = harness
        .post_form(
            "/api/versions_list",
            &[("name", "photos/abstract/v.png")],
        )
        .await?;
    let n_after = after
        .get("versions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(n_after, 1, "expected one archived version after PUT-replace; got: {after}");
    harness.close().await
}

#[tokio::test]
async fn restore_roundtrips_original_bytes() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;

    let original = holofs_e2e::fixtures::textured_image_png();
    let replacement = holofs_e2e::fixtures::tiny_image_png();

    // PUT the original. Stage the data_cid so we can identify the
    // archived row deterministically — `versions_list[0].id` will
    // contain the first 8 hex chars of the data_cid.
    let orig_put = harness
        .put_bytes("photos/abstract/r.png", original.to_vec())
        .await?;
    let orig_cid_hex = orig_put
        .get("data_cid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // What the gateway encoded for `original` is what GET returns;
    // we want to restore-match against that, not against the
    // pre-encode source bytes (the gateway downscales / re-encodes
    // every image at PUT time).
    let canonical = harness.get_bytes("photos/abstract/r.png").await?;

    // PUT-replace.
    harness
        .put_bytes("photos/abstract/r.png", replacement.to_vec())
        .await?;

    let listing = harness
        .post_form("/api/versions_list", &[("name", "photos/abstract/r.png")])
        .await?;
    let archived_id = listing
        .get("versions")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .expect("expected a version archive after PUT-replace");
    assert!(
        archived_id.contains(&orig_cid_hex[..8]),
        "archived id {archived_id} does not embed original cid prefix {}",
        &orig_cid_hex[..8]
    );

    // /api/restore is a leptos server fn — POST form body.
    let client = reqwest::Client::new();
    let resp = client
        .post(harness.url("/api/restore"))
        .form(&[("name", "photos/abstract/r.png"), ("id", archived_id.as_str())])
        .send()
        .await?;
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 303,
        "/api/restore returned HTTP {}",
        resp.status()
    );

    let restored = harness.get_bytes("photos/abstract/r.png").await?;
    assert_eq!(
        restored, canonical,
        "restored bytes differ from pre-replace canonical decode"
    );
    harness.close().await
}

#[tokio::test]
async fn versions_page_renders_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    harness
        .put_bytes(
            "photos/abstract/v.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    harness
        .put_bytes(
            "photos/abstract/v.png",
            holofs_e2e::fixtures::tiny_image_png().to_vec(),
        )
        .await?;
    harness.goto("/versions/photos/abstract/v.png").await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    assert!(
        body.contains("v.png") || body.contains("versions") || body.contains("версии"),
        "/versions page does not surface the subject filename or page label: {body:.200}"
    );
    harness.close().await
}
