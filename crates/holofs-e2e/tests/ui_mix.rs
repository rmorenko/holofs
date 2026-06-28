//! `/mix?a=<image>` wavelet composer (Stage 12.6).
//!
//! Form-driven: pick a second image, pick a split point, render
//! the composite at `/api/mix.png`. The interesting cross-check is
//! that the rendered PNG actually depends on the split parameter.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn mix_page_loads_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let path = harness.seed_textured_image().await?;
    harness.goto(&format!("/mix?a={path}")).await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    // The mix page renders form controls — at minimum a select /
    // input for the second image plus a split slider.
    let inputs = harness.driver.find_all(By::Css("input, select")).await?;
    assert!(
        !inputs.is_empty(),
        "/mix page rendered no <input>/<select> controls"
    );
    harness.close().await
}

#[tokio::test]
async fn mix_png_depends_on_split_parameter() -> Result<()> {
    // Without two images you can't actually mix; for this we need
    // two distinct PNGs (different bytes → different data_cid).
    let harness = TestHarness::fresh().await?;
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
            holofs_e2e::fixtures::tiny_image_png().to_vec(),
        )
        .await?;
    // Split at L0 (almost-all-A) vs L3 (almost-all-B) must produce
    // visibly different composites.
    let early = harness
        .get_bytes("/api/mix.png?a=photos/abstract/a.png&b=photos/abstract/b.png&split=0")
        .await?;
    let late = harness
        .get_bytes("/api/mix.png?a=photos/abstract/a.png&b=photos/abstract/b.png&split=3")
        .await?;
    assert!(
        early.len() > 100 && late.len() > 100,
        "mix PNGs suspiciously tiny (early={}, late={})",
        early.len(),
        late.len()
    );
    assert_ne!(
        early, late,
        "/api/mix.png returned identical bytes for split=0 vs split=3; \
         the wavelet-mix split control is not affecting the composite"
    );
    harness.close().await
}
