//! `/inspect/<name>` visual shard audit grid + `/inspect-zoom/...`
//! per-shard zoom page (Stages 11.2 / 12+).

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn inspect_grid_renders_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    harness.goto("/inspect/photos/abstract/mandala.png").await?;
    // Inspect renders 444 thumbnails for an image manifest — that
    // many lazy <img>s means hydration has more work to do than
    // most pages. Be patient.
    harness.wait_for("img", Duration::from_secs(15)).await?;
    let imgs = harness.driver.find_all(By::Css("img")).await?;
    assert!(
        !imgs.is_empty(),
        "/inspect did not render any <img> elements"
    );
    // Be permissive on exact count: the seeded fixture is a tiny
    // 32×32 solid colour, the gateway upscales it to its native
    // base resolution (3 channels × 4 layers × varying RLNC width).
    // We only require enough to prove the grid layout fired.
    assert!(
        imgs.len() >= 10,
        "expected the inspect grid to render ≥ 10 shard thumbnails, got {}",
        imgs.len()
    );
    harness.close().await
}

#[tokio::test]
async fn inspect_zoom_for_a_known_shard_returns_200() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    // `(channel, layer, idx) = (0, 0, 0)` is the very first
    // systematic shard of any image manifest — guaranteed to exist
    // for the seeded mandala.
    let url = harness.url("/inspect-zoom/0_0_0/photos/abstract/mandala.png");
    let resp = reqwest::Client::new().get(&url).send().await?;
    assert!(
        resp.status().is_success(),
        "/inspect-zoom returned HTTP {}",
        resp.status()
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("mandala") || body.contains("/api/shard/"),
        "inspect-zoom body does not reference the shard or its source image: {body:.200}"
    );
    harness.close().await
}
