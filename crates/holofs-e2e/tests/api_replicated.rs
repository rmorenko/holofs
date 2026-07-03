//! Stage 15.1: HTTP surface for the per-block Replicated encoding.
//!
//! The Rust API (`Gateway::ingest_bytes_replicated`) is covered by
//! the CLI-level distributed integration tests. This suite pins the
//! HTTP entry point: `PUT /*path?encoding=replicated&…` routes to
//! the block encoder and the resulting object is retrievable via
//! the same GET path as the RLNC default.

use anyhow::Result;
use holofs_e2e::TestHarness;
use reqwest::StatusCode;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

/// PUT with `?encoding=replicated` succeeds for an image body and
/// the object round-trips via GET. The block-encoded manifest is
/// stored + retrieved without touching the caller-facing byte
/// stream.
#[tokio::test]
async fn put_replicated_image_ingests_and_reads_back() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/replicated.png?encoding=replicated&block_size=64&r=3"))
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "replicated PUT should succeed for an image body"
    );
    // GET returns the reconstructed image (bytes may differ — the
    // gateway re-encodes as PNG on decode). Confirm the status +
    // content-type; byte-identity isn't the contract here.
    let resp = raw_client()
        .get(harness.url("photos/replicated.png"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("image/"), "GET should return an image, got {ct:?}");
    harness.close().await
}

/// PUT with `?encoding=replicated` and default knobs (block_size +
/// r omitted) succeeds — the handler applies the documented
/// defaults (64, 3).
#[tokio::test]
async fn put_replicated_defaults_apply_when_knobs_omitted() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/defaults.png?encoding=replicated"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    harness.close().await
}

/// The explicit `encoding=rlnc` alias is accepted (backward
/// compatibility with a client that opts in defensively).
#[tokio::test]
async fn put_explicit_rlnc_is_accepted() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/explicit-rlnc.png?encoding=rlnc"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    harness.close().await
}

/// Unknown `?encoding=` values are 400 with a diagnostic body —
/// silently ignoring an unrecognised knob would let a typo
/// (`encoding=replicate`) reach the RLNC path unnoticed.
#[tokio::test]
async fn put_unknown_encoding_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/nope.png?encoding=replicate"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = resp.text().await.unwrap_or_default();
    assert!(
        msg.contains("replicate") || msg.contains("encoding"),
        "400 body should reference the bad knob, got {msg:?}"
    );
    harness.close().await
}

/// Non-numeric `?block_size=abc` → 400 (parser failure, not
/// silently defaulted).
#[tokio::test]
async fn put_bad_block_size_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/badbs.png?encoding=replicated&block_size=abc"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}
