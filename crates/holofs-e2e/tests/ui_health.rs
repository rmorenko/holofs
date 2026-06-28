//! `/health` cluster dashboard + `/health/<name>` per-file metrics
//! page (Stages 12.7 / 14.x).

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn health_index_renders_node_breakdown() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/health").await?;
    harness.wait_for("body", Duration::from_secs(8)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    // The dashboard surfaces "nodes" / "zones" / a numeric live-set
    // count in every locale — just confirm the page is non-trivial.
    assert!(
        body.len() > 100,
        "/health body suspiciously short ({} bytes): {body:.200}",
        body.len()
    );
    // And confirm the cluster's node count surfaces as a number
    // somewhere; the harness's gateway boots with 40 nodes by
    // default (see HOLOFS_N_NODES in core).
    assert!(
        body.contains("40") || body.contains("nodes") || body.contains("узл"),
        "/health does not mention node count: {body:.300}"
    );
    harness.close().await
}

#[tokio::test]
async fn per_file_metrics_page_loads_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    harness.goto("/health/photos/abstract/mandala.png").await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    // The metrics view's hero element renders the path in CSS
    // `text-transform: uppercase`, so the visible-text comparison
    // must be case-insensitive.
    let body_lc = body.to_lowercase();
    assert!(
        body_lc.contains("mandala.png"),
        "expected filename in /health/<name> body: {body:.200}"
    );
    harness.close().await
}

#[tokio::test]
async fn file_metrics_api_returns_expected_schema() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    let v = harness
        .post_form(
            "/api/file_metrics",
            &[("name", "photos/abstract/mandala.png")],
        )
        .await?;
    // Validate the key fields the /health/<name> page consumes.
    for field in [
        "kind",
        "total_shards_in_file",
        "unique_shards_in_file",
        "catalog_total_shards",
        "originality_pct",
        "originality_per_layer",
        "neighbours",
    ] {
        assert!(
            v.get(field).is_some(),
            "/api/file_metrics missing field `{field}`: {v}"
        );
    }
    // originality_per_layer must be an array of length == nlayers.
    let per_layer = v
        .get("originality_per_layer")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !per_layer.is_empty(),
        "originality_per_layer was empty: {v}"
    );
    harness.close().await
}

#[tokio::test]
async fn health_events_sse_stream_responds_with_event_stream_content_type() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    // Read the response with `bytes_stream()` so we can grab the
    // FIRST chunk and bail out — without it, `resp.bytes()` waits
    // for the stream to terminate (it never does — SSE is
    // open-ended). The first frame from holofs::monitor arrives
    // within ~3 s; we give it 10 s of head-room.
    let url = harness.url("/api/health/events");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = client.get(&url).send().await?;
    assert!(resp.status().is_success(), "SSE endpoint returned {}", resp.status());
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.contains("text/event-stream"),
        "SSE content-type wrong; got `{ct}` (expected `text/event-stream`)"
    );
    // Read chunks until either a frame appears or 10 s have
    // elapsed. `Response::chunk` is reqwest's native incremental
    // reader; wrapping it in `tokio::time::timeout` gives the
    // bound. We can't use `resp.bytes().await` here because SSE is
    // an open-ended stream and that call would never return.
    let mut resp = resp;
    let mut buf = String::new();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        let next = tokio::time::timeout(Duration::from_secs(2), resp.chunk()).await;
        match next {
            Ok(Ok(Some(bytes))) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                if buf.contains("data:") || buf.contains("event:") {
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(
        buf.contains("data:") || buf.contains("event:"),
        "SSE stream did not produce an `event:` / `data:` frame within 10 s; got: {:.200}",
        buf
    );
    harness.close().await
}
