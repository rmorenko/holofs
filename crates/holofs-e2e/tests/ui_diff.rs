//! `/diff?a=…&b=…` chunk-diff page (Stage 9 + 11.16).
//!
//! Regression guard against a user-reported bug where clicking the
//! `diff →` action from `/similar/<name>` rendered the title
//! correctly but the body showed "not found". Root cause: the
//! `diff →` link in `similar.rs` had no `rel="external"`, so the
//! leptos Router intercepted the click, the Suspense resource was
//! stale, and the new (a, b) pair was effectively resolved against
//! an empty catalog snapshot.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn direct_url_navigation_renders_diff_body() -> Result<()> {
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
    harness
        .goto("/diff?a=photos/abstract/a.png&b=photos/abstract/b.png")
        .await?;
    harness
        .wait_until(
            async |drv| {
                let body = drv.find(By::Css("body")).await?.text().await?;
                let lc = body.to_lowercase();
                Ok(
                    if lc.contains("chunk diff") && lc.contains("identical") {
                        Some(())
                    } else {
                        None
                    },
                )
            },
            Duration::from_secs(15),
        )
        .await?;
    harness.close().await
}

#[tokio::test]
async fn click_from_similar_navigates_to_diff_with_data() -> Result<()> {
    // The bug we're guarding against: clicking the `diff →` action
    // on `/similar/<a>` had no `rel="external"`, so leptos Router
    // intercepted the click and the resulting `/diff` page showed
    // an empty "not found" body instead of the real diff table.
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

    harness.goto("/similar/photos/abstract/a.png").await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;

    // Find the action-row `diff →` link aimed at b.png.
    let diff_link = harness
        .wait_until(
            async |drv| {
                let links = drv.find_all(By::Css("a")).await?;
                for a in links {
                    let href = a.attr("href").await.unwrap_or_default().unwrap_or_default();
                    if href.contains("/diff?") && href.contains("b=photos/abstract/b.png") {
                        return Ok(Some(a));
                    }
                }
                Ok(None)
            },
            Duration::from_secs(10),
        )
        .await?;
    diff_link.click().await?;

    // After a full-reload navigation, URL is /diff?a=…&b=…, body
    // contains the diff table. Without rel="external" the Router
    // would intercept and the assertion below would catch
    // either a still-on-/similar URL or an empty "not found" body.
    harness
        .wait_until(
            async |drv| {
                let url = drv.current_url().await?;
                let body = drv.find(By::Css("body")).await?.text().await?;
                let lc = body.to_lowercase();
                let ok = url.path() == "/diff"
                    && lc.contains("chunk diff")
                    && lc.contains("identical");
                Ok(if ok { Some(()) } else { None })
            },
            Duration::from_secs(15),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "click on /similar/<a>'s `diff →` link did not land on a populated \
                 /diff page within 15s. Either the link is missing rel=\"external\" \
                 (leptos Router intercepted the click and the Suspense resource saw \
                 stale params) or the diff backend returned NotFound for the seeded \
                 pair."
            )
        })?;
    harness.close().await
}
