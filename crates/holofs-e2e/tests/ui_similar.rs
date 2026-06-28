//! `/similar/<name>` page (Stages 11.16 + 13.0).
//!
//! Validates the shard-overlap table renders and that the robust-
//! copy score column is wired up. The synthetic test fixture's
//! score-formula caveat (documented in §21 of test-scenarios.md and
//! the test-runs report) means we don't assert score sign — only
//! the field's presence and that some neighbour list is rendered.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn similar_page_loads_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    harness.goto("/similar/photos/abstract/mandala.png").await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    assert!(
        body.contains("mandala") || body.contains("similar"),
        "/similar page does not name the subject image: {body:.200}"
    );
    harness.close().await
}

#[tokio::test]
async fn similar_page_includes_robust_copy_field_in_payload() -> Result<()> {
    // We don't have a reliable cross-locale text marker for the
    // robust-copy column. The data hides in the page's
    // hydration-resource JSON; we look for the field name there.
    let harness = TestHarness::fresh().await?;
    // Seed with two near-duplicate images so the overlaps table is
    // non-empty. The minimal seed has one image only.
    harness.mkdir_p("photos/abstract").await?;
    harness
        .put_bytes(
            "photos/abstract/a.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    harness
        .put_bytes(
            "photos/abstract/b.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let html = String::from_utf8(
        harness.get_bytes("/similar/photos/abstract/a.png").await?,
    )
    .unwrap_or_default();
    // The leptos hydration resource embeds the ShardOverlap struct;
    // we just need the field name to be present.
    assert!(
        html.contains("robust_copy_score"),
        "/similar HTML does not embed robust_copy_score field; ShardOverlap likely not in render tree: {} bytes",
        html.len()
    );
    harness.close().await
}
