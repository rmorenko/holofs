//! Batch H: search semantics.
//!
//! Existing `ui_search.rs` proves the page renders; this suite proves
//! the underlying API contract: 400 / 503 on misuse, real ranking
//! sanity when CLIP is enabled, and "post-then-search" eventual
//! consistency on the embedding index.

use std::time::Duration;

use anyhow::{anyhow, Result};
use holofs_e2e::{HarnessConfig, TestHarness};
use reqwest::StatusCode;
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

fn embedding_config() -> HarnessConfig {
    HarnessConfig {
        enable_embed: true,
        ..HarnessConfig::default()
    }
}

// === Negative paths (no CLIP needed) ======================================

/// /api/search without the `q` query param → 400. Catches handler-
/// level URL parsing regressions.
#[tokio::test]
async fn search_missing_q_param_returns_400() -> Result<()> {
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    let resp = raw_client()
        .get(harness.url("/api/search"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

/// /api/search?q= with an empty / whitespace-only query → 400.
/// The handler explicitly rejects empty queries before paying the
/// CLIP-encode cost.
#[tokio::test]
async fn search_with_empty_query_returns_400() -> Result<()> {
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    let resp = raw_client()
        .get(harness.url("/api/search?q=%20%20"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

/// /api/search when the gateway was NOT booted with --enable-embed
/// → 503 with a hint. Default `HarnessConfig` has embed off, so we
/// reuse it directly here.
#[tokio::test]
async fn search_when_embed_disabled_returns_503() -> Result<()> {
    // Explicit default — embed off.
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .get(harness.url("/api/search?q=ocean"))
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "search without embed should 503"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.to_lowercase().contains("embed") || body.to_lowercase().contains("--enable"),
        "503 body should hint at the missing flag, got {body:?}"
    );
    harness.close().await
}

// === Ranking semantics (requires CLIP — opt in via --include-ignored) =====

/// Ranking sanity: against the standard search corpus (mountain /
/// ocean / sunset / forest / snow / desert), the query "ocean"
/// must rank `ocean.png` higher than `mountain.png`. CLIP is far
/// from perfect on tiny solid-color PNGs, but ocean is closer in
/// embedding space to "ocean" than mountain is by a clear margin.
#[tokio::test]
#[ignore = "downloads ~155 MiB of CLIP weights on first run; opt in via --include-ignored"]
async fn search_ranks_ocean_above_mountain_for_ocean_query() -> Result<()> {
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    harness.seed_for_search().await?;
    let resp = raw_client()
        .get(harness.url("/api/search?q=ocean&limit=10&band=any"))
        .send()
        .await?
        .error_for_status()?;
    let v: Value = serde_json::from_str(&resp.text().await?)?;
    let hits = v["hits"].as_array().ok_or_else(|| anyhow!("no hits array"))?;
    let mut ocean_score: Option<f64> = None;
    let mut mountain_score: Option<f64> = None;
    for h in hits {
        let name = h["name"].as_str().unwrap_or("");
        let score = h["score"].as_f64().unwrap_or(f64::MIN);
        if name.ends_with("ocean.png") && ocean_score.is_none() {
            ocean_score = Some(score);
        }
        if name.ends_with("mountain.png") && mountain_score.is_none() {
            mountain_score = Some(score);
        }
    }
    let o = ocean_score.ok_or_else(|| anyhow!("ocean.png missing from hits"))?;
    let m = mountain_score.ok_or_else(|| anyhow!("mountain.png missing from hits"))?;
    assert!(
        o > m,
        "ranking sanity: 'ocean' should score ocean.png ({o}) above mountain.png ({m})"
    );
    harness.close().await
}

/// Post-then-search: after a PUT, the new object should become
/// findable within a reasonable polling window. The embedding
/// index is built lazily — first search after the PUT triggers a
/// rebuild. We don't pin a specific latency budget, just bound it
/// at ~10 s.
#[tokio::test]
#[ignore = "downloads ~155 MiB of CLIP weights on first run; opt in via --include-ignored"]
async fn search_finds_freshly_put_object_eventually() -> Result<()> {
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    harness.seed_for_search().await?;
    harness.mkdir_p("photos/abstract").await?;
    harness
        .put_bytes(
            "photos/abstract/desert-twin.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let resp = raw_client()
            .get(harness.url("/api/search?q=desert&limit=20&band=any"))
            .send()
            .await?
            .error_for_status()?;
        let v: Value = serde_json::from_str(&resp.text().await?)?;
        let hits = v["hits"].as_array().cloned().unwrap_or_default();
        let found = hits.iter().any(|h| {
            h["name"].as_str().map(|s| s.ends_with("desert-twin.png")).unwrap_or(false)
        });
        if found {
            return harness.close().await;
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "desert-twin.png never showed up in /api/search?q=desert within 10s"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
