//! Batch A: negative-path coverage for the HTTP API.
//!
//! Every test here makes a *malformed* or *out-of-contract* request
//! and asserts the gateway returns the right 4xx — not a 5xx and
//! never a panic. The error map lives in
//! `holofs-web::handlers::error_to_response`; this suite is its
//! integration counterpart.

use anyhow::Result;
use holofs_e2e::TestHarness;
use reqwest::StatusCode;

/// Build a plain reqwest client. Most negative-path tests want the
/// raw status code, not the JSON-decoded response — so we sidestep
/// the harness helpers and POST/PUT/GET directly.
fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

// === PUT ===================================================================

/// PUT with an empty body is meaningless — the gateway must reject
/// it instead of writing a zero-shard manifest.
#[tokio::test]
async fn put_empty_body_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .put(harness.url("photos/empty.png"))
        .body(Vec::<u8>::new())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

/// PUT to a path where a directory already lives must NOT clobber
/// the directory — return 409 AlreadyExists.
#[tokio::test]
async fn put_over_existing_directory_returns_409() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("photos/abstract"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    harness.close().await
}

/// PUT to a path whose parent directory doesn't exist is a 400 — the
/// gateway requires the parent to be created first via /api/mkdir.
#[tokio::test]
async fn put_to_missing_parent_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = raw_client()
        .put(harness.url("no-such-dir/photo.png"))
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

/// Path traversal: `..` segments must be rejected. HTTP-layer URL
/// normalisation usually collapses `..` for us, so the test attack
/// vector goes through a form field — the gateway sees `..` raw and
/// must reject it inside `catalog_path::validate`.
#[tokio::test]
async fn mkdir_with_dotdot_segment_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .post(harness.url("/api/mkdir"))
        .form(&[("parent", "photos"), ("name", ".."), ("return_to", "/")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

// === GET ===================================================================

/// GET on a name that's not in the catalog → 404.
#[tokio::test]
async fn get_missing_object_returns_404() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .get(harness.url("photos/ghost.png"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}

/// GET on a directory entry has no payload to serve — gateway must
/// 409 instead of silently returning an empty body.
#[tokio::test]
async fn get_on_directory_returns_409() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let resp = raw_client()
        .get(harness.url("photos/abstract"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    harness.close().await
}

/// `/preview/<text-or-opaque>` has no graceful projection — 404.
/// Images and audio have previews (LL band, audio thumb); text and
/// opaque do not.
#[tokio::test]
async fn preview_of_text_object_returns_404() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("docs").await?;
    harness.put_text("docs/notes.txt", "hello world").await?;
    let resp = raw_client()
        .get(harness.url("preview/docs/notes.txt"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}

// === DELETE ================================================================

/// DELETE on a name that doesn't exist returns 404 — not 200 / silent.
/// Lets clients distinguish "deleted" from "wasn't there".
#[tokio::test]
async fn delete_missing_object_returns_404() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .delete(harness.url("photos/ghost.png"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}

// === mkdir =================================================================

/// mkdir over an existing file → 409 NotADirectory (the slot is
/// occupied by a non-directory entry, can't promote).
#[tokio::test]
async fn mkdir_over_existing_file_returns_4xx() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/file.png", body).await?;
    let resp = raw_client()
        .post(harness.url("/api/mkdir"))
        .form(&[("parent", "photos"), ("name", "file.png"), ("return_to", "/")])
        .send()
        .await?;
    let status = resp.status();
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::BAD_REQUEST,
        "mkdir over file: expected 4xx, got {status}"
    );
    harness.close().await
}

// === rmdir =================================================================

/// rmdir on a directory that still has children → 409 DirectoryNotEmpty.
/// Forces the caller to remove the contents first.
#[tokio::test]
async fn rmdir_non_empty_directory_returns_409() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/abstract/x.png", body).await?;
    let resp = raw_client()
        .post(harness.url("/api/rmdir"))
        .form(&[("path", "photos/abstract"), ("return_to", "/")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    harness.close().await
}

/// rmdir on a non-directory entry → 4xx (NotADirectory or BadRequest).
#[tokio::test]
async fn rmdir_on_file_returns_4xx() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/x.png", body).await?;
    let resp = raw_client()
        .post(harness.url("/api/rmdir"))
        .form(&[("path", "photos/x.png"), ("return_to", "/")])
        .send()
        .await?;
    let status = resp.status();
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::BAD_REQUEST,
        "rmdir on file: expected 4xx, got {status}"
    );
    harness.close().await
}

/// rmdir on a name that doesn't exist → 404.
#[tokio::test]
async fn rmdir_missing_directory_returns_404() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .post(harness.url("/api/rmdir"))
        .form(&[("path", "photos/never-made"), ("return_to", "/")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}
