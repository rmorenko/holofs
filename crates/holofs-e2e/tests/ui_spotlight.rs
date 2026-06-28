//! `/spotlight?a=<image>` ROI composer (Stages 13.2 + 14.1).
//!
//! The page presents a target image plus a form for the ROI
//! (x, y, w, h fractions ∈ [0, 1]) and a mode toggle
//! (`spatial` vs `coeff`). The result image lives at
//! `/api/spotlight.png?...` and is the side that's easiest to
//! verify end-to-end without scripting the form.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn spotlight_page_renders_for_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let path = harness.seed_textured_image().await?;
    harness.goto(&format!("/spotlight?a={path}")).await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    // Some <input>s should appear — the ROI form's fractional
    // x/y/w/h fields.
    let inputs = harness.driver.find_all(By::Css("input")).await?;
    assert!(
        !inputs.is_empty(),
        "/spotlight page rendered no <input> elements"
    );
    harness.close().await
}

#[tokio::test]
async fn spatial_and_coeff_modes_produce_distinct_pngs() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let path = harness.seed_textured_image().await?;
    let make_url = |mode: &str| {
        format!(
            "/api/spotlight.png?name={path}&x=0.3&y=0.3&w=0.4&h=0.4&mode={mode}"
        )
    };
    let spatial = harness.get_bytes(&make_url("spatial")).await?;
    let coeff = harness.get_bytes(&make_url("coeff")).await?;
    assert!(
        spatial.len() > 100 && coeff.len() > 100,
        "spotlight PNGs suspiciously tiny (spatial={}, coeff={})",
        spatial.len(),
        coeff.len()
    );
    // Both must start with the PNG magic.
    assert_eq!(
        &spatial[..4],
        &[0x89, b'P', b'N', b'G'],
        "spatial output is not a PNG"
    );
    assert_eq!(
        &coeff[..4],
        &[0x89, b'P', b'N', b'G'],
        "coeff output is not a PNG"
    );
    // The two modes draw *the same image* in different ways; the
    // pixel bytes between them must therefore differ.
    assert_ne!(
        spatial, coeff,
        "spatial and coeff spotlight modes produced byte-identical PNGs"
    );
    harness.close().await
}

#[tokio::test]
async fn spotlight_response_carries_diagnostic_headers() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let path = harness.seed_textured_image().await?;
    let url = harness.url(&format!(
        "/api/spotlight.png?name={path}&x=0.25&y=0.25&w=0.5&h=0.5&mode=coeff"
    ));
    let resp = reqwest::Client::new().get(&url).send().await?;
    assert!(resp.status().is_success(), "spotlight returned {}", resp.status());
    let headers = resp.headers().clone();
    for name in [
        "x-holofs-roi-px",
        "x-holofs-decode-ms",
        "x-holofs-bytes-downloaded",
    ] {
        assert!(
            headers.contains_key(name),
            "spotlight response missing `{name}` header; got: {headers:?}"
        );
    }
    harness.close().await
}
