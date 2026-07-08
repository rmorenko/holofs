//! Async ingest (`HOLOFS_ASYNC_ENCODE=1`): 202 Accepted PUT +
//! `Retry-After` polling contract.
//!
//! The runtime shape is that a PUT under the async flag stages a
//! placeholder `state = Encoding` manifest, returns 202 immediately,
//! and a background worker flips the state to `Ready` (or `Failed`)
//! once encoding finishes. These tests pin that contract from the
//! outside so future refactors don't drift:
//!
//! * 202 + `Location` + `state="encoding"` on the initial PUT.
//! * GET during `Encoding` yields `503 + Retry-After: 5`.
//! * A polling loop eventually observes 200 with the round-tripped
//!   bytes.
//! * A second concurrent PUT for the same name during `Encoding`
//!   yields `409 + Retry-After: 5` — a transient conflict, distinct
//!   from the terminal `AlreadyExists` that a fully-Ready object
//!   surfaces.
//! * DELETE during `Encoding` yields `409` — the caller must wait
//!   for the worker to settle before removing the manifest.
//! * A restart while an async encode is in flight downgrades the
//!   placeholder to `Failed` (bootstrap recovery), and the follow-up
//!   GET surfaces the specific "previous PUT failed to encode" 404
//!   so the caller knows the name is safe to PUT again.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use holofs_e2e::{HarnessConfig, TestHarness};
use reqwest::StatusCode;
use serde_json::Value;

/// Reqwest client that does NOT auto-follow redirects. The default
/// client would follow 202 → GET / whatever the browser would do,
/// which hides the 202 status code from the assertions.
fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

fn async_harness_config() -> HarnessConfig {
    let mut cfg = HarnessConfig::default();
    cfg.extra_env
        .push(("HOLOFS_ASYNC_ENCODE".into(), "1".into()));
    cfg
}

async fn spawn_async_gateway() -> Result<TestHarness> {
    TestHarness::fresh_with(async_harness_config()).await
}

/// Poll `GET name` until the manifest reaches a terminal state.
/// Returns the final response. Bounded at 30s so a stuck worker
/// fails the test instead of hanging the run.
async fn poll_until_ready(
    client: &reqwest::Client,
    harness: &TestHarness,
    name: &str,
) -> Result<reqwest::Response> {
    let url = harness.url(name);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let resp = client.get(&url).send().await?;
        let status = resp.status();
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return Ok(resp);
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "GET {name} stayed in Encoding (503) for >30s — worker stuck"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// === 202 Accepted contract =================================================

/// PUT under the async flag returns 202 with `Location: /<name>` and a
/// JSON body advertising `state:"encoding"`. This is the wire-level
/// contract every polling client depends on.
#[tokio::test]
async fn put_async_returns_202_with_location_and_encoding_state() -> Result<()> {
    let harness = spawn_async_gateway().await?;
    harness.mkdir_p("photos").await?;
    let client = raw_client();
    let name = "photos/async.png";
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let resp = client
        .put(harness.url(name))
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "async PUT must return 202 Accepted, got {}",
        resp.status()
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(
        location,
        format!("/{name}"),
        "202 Location must point at the polling URL, got {location:?}"
    );
    let json: Value = resp.json().await?;
    assert_eq!(
        json["state"].as_str(),
        Some("encoding"),
        "202 body must advertise state=encoding, got {json}"
    );
    assert_eq!(json["name"].as_str(), Some(name));

    // The polling loop must eventually see the bytes.
    let ready = poll_until_ready(&client, &harness, name).await?;
    assert_eq!(ready.status(), StatusCode::OK, "eventual GET must be 200");
    let got = ready.bytes().await?;
    assert!(
        got.starts_with(&[0x89, b'P', b'N', b'G']),
        "final decode must be a valid PNG (len={})",
        got.len()
    );
    harness.close().await
}

/// A GET issued while the manifest is still `Encoding` must return
/// 503 + `Retry-After: 5`. Together with the 202, this is the
/// polling contract the soak runner + external clients rely on.
///
/// The encode is fast enough that we may race past `Encoding` before
/// the first GET — in that case the assertion silently passes on the
/// 200 branch, and the eventual-consistency invariant is still
/// verified by the terminal poll below. Absence-of-503 is not a bug;
/// the presence of a non-{200,503} status IS a bug.
#[tokio::test]
async fn get_during_encoding_returns_503_with_retry_after() -> Result<()> {
    let harness = spawn_async_gateway().await?;
    harness.mkdir_p("photos").await?;
    let client = raw_client();
    let name = "photos/mid-encode.png";
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    client
        .put(harness.url(name))
        .body(body)
        .send()
        .await?
        .error_for_status()?;
    // Best-effort race: fire a GET immediately after the 202 lands.
    // If we catch the Encoding window, verify the header shape; if
    // the encode already finished (fast machine), just fall through
    // to the terminal poll.
    let resp = client.get(harness.url(name)).send().await?;
    let status = resp.status();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            retry_after, "5",
            "503 during Encoding must include Retry-After: 5, got {retry_after:?}"
        );
    } else {
        assert_eq!(
            status,
            StatusCode::OK,
            "GET during encoding must be 503 or 200 (raced past), got {status}"
        );
    }
    // The manifest must reach Ready within the timeout regardless.
    let ready = poll_until_ready(&client, &harness, name).await?;
    assert_eq!(ready.status(), StatusCode::OK);
    harness.close().await
}

// === Idempotency + conflict semantics =====================================

/// A second PUT for the same name while the first async ingest is
/// still running must return `409 + Retry-After: 5`. Distinct from
/// the terminal `AlreadyExists` (which also is 409 but carries no
/// `Retry-After`) — a polling client needs to keep retrying, not
/// abandon.
///
/// Race note: same as the GET-during-encoding case, the encode may
/// finish before the second PUT lands. The test only asserts when
/// we win the race; on the fast path the second PUT sees a Ready
/// manifest and returns 201/409 without Retry-After, which is
/// acceptable behaviour.
#[tokio::test]
async fn second_put_during_encoding_returns_409_with_retry_after() -> Result<()> {
    let harness = spawn_async_gateway().await?;
    harness.mkdir_p("photos").await?;
    let client = raw_client();
    let name = "photos/racing.png";
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    // First PUT — 202.
    let first = client
        .put(harness.url(name))
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    // Second PUT immediately after. If the placeholder is still
    // Encoding we expect 409 + Retry-After: 5.
    let second = client
        .put(harness.url(name))
        .body(body.clone())
        .send()
        .await?;
    let status = second.status();
    if status == StatusCode::CONFLICT {
        let retry_after = second
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            retry_after, "5",
            "409 during Encoding must include Retry-After: 5, got {retry_after:?}"
        );
    } else {
        // Racing past Encoding is legal; assert only that we didn't
        // land in a nonsense state.
        assert!(
            status == StatusCode::CREATED
                || status == StatusCode::ACCEPTED
                || status == StatusCode::CONFLICT,
            "second PUT status must be 202/201/409, got {status}"
        );
    }
    // Drain the encode so `Drop` doesn't race the still-live worker.
    let _ = poll_until_ready(&client, &harness, name).await?;
    harness.close().await
}

/// DELETE during Encoding must return 409. The manifest is still
/// mid-flight; removing it now would leak the placeholder + orphan
/// the spawned worker, so the gateway refuses.
#[tokio::test]
async fn delete_during_encoding_returns_409() -> Result<()> {
    let harness = spawn_async_gateway().await?;
    harness.mkdir_p("photos").await?;
    let client = raw_client();
    let name = "photos/pending-delete.png";
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    client
        .put(harness.url(name))
        .body(body)
        .send()
        .await?
        .error_for_status()?;
    let del = client.delete(harness.url(name)).send().await?;
    let status = del.status();
    // On the winning-race branch we assert 409. On the losing-race
    // branch (encode raced to Ready before the DELETE landed) we
    // accept the terminal 200 that DELETE-on-Ready normally returns.
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::OK,
        "DELETE during encoding must be 409 (still encoding) or 200 (raced past to Ready), got {status}"
    );
    // Drain: whichever branch we hit, wait for the manifest to reach
    // a terminal state before dropping the harness.
    let _ = poll_until_ready(&client, &harness, name).await;
    harness.close().await
}

// === Bootstrap recovery ===================================================

/// A gateway killed while an async encode is in flight leaves a
/// placeholder `state = Encoding` manifest on disk. On restart,
/// `bootstrap::recover_encoding_manifests` must downgrade it to
/// `Failed`. The follow-up GET surfaces the specific
/// "previous PUT failed to encode; PUT again to replace" 404 so
/// callers can tell terminal-Failed apart from name-never-existed.
#[tokio::test]
async fn restart_downgrades_in_flight_encoding_to_failed() -> Result<()> {
    let mut harness = spawn_async_gateway().await?;
    harness.mkdir_p("photos").await?;
    let client = raw_client();
    let name = "photos/interrupted.png";
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let put = client
        .put(harness.url(name))
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(put.status(), StatusCode::ACCEPTED);
    // Kill + respawn IMMEDIATELY — the worker probably had no time
    // to finish. If it did (fast machine), the manifest reaches
    // Ready and the test still checks that restart survived the
    // encode; we branch on the post-restart state below.
    harness.restart().await?;
    let post = client.get(harness.url(name)).send().await?;
    let status = post.status();
    if status == StatusCode::NOT_FOUND {
        // Winning-race path: encoding was in flight when we killed
        // the gateway; bootstrap downgraded it to Failed. The body
        // must mention re-PUT to distinguish from a plain 404.
        let diag = post.text().await?;
        assert!(
            diag.contains("PUT again") || diag.contains("failed"),
            "Failed-state 404 body should hint at re-PUT, got {diag:?}"
        );
        // A fresh PUT under the same name must now succeed (the
        // placeholder is gone).
        let redo = client
            .put(harness.url(name))
            .body(body.clone())
            .send()
            .await?;
        assert!(
            redo.status() == StatusCode::CREATED
                || redo.status() == StatusCode::ACCEPTED,
            "re-PUT after Failed must succeed, got {}",
            redo.status()
        );
        let _ = poll_until_ready(&client, &harness, name).await?;
    } else {
        // Losing-race path: worker beat us to Ready before the kill,
        // and the manifest survived the restart intact.
        assert_eq!(
            status,
            StatusCode::OK,
            "post-restart GET must be 200 (encode completed pre-kill) or 404 (Failed after recovery), got {status}"
        );
    }
    harness.close().await
}
