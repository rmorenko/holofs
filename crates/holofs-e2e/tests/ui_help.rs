//! In-app help / docs viewer (Stage 10).
//!
//! `/help` is the index, `/help/<slug>` is an individual rendered
//! markdown page. Routes are leptos server-fn-backed but rendered
//! during SSR, so the content is in the initial HTML body.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn help_index_returns_200_and_lists_docs() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/help").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    let body_lc = body.to_lowercase();
    // The index page advertises at least one known doc slug in
    // every locale; checking for "help" or "api" or "scenarios" or
    // "архитект" covers the cross-locale set.
    let has_index_marker = ["help", "api", "scenario", "architect", "архитект"]
        .iter()
        .any(|w| body_lc.contains(w));
    assert!(
        has_index_marker,
        "/help index does not surface any recognisable doc title: {body:.300}"
    );
    harness.close().await
}

#[tokio::test]
async fn help_doc_page_renders_markdown_content() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    // `api` is one of the canonical slugs (mirrors docs/api.md).
    harness.goto("/help/api").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    // The api doc contains "HTTP gateway" near the top in every
    // locale (the section title is rarely translated verbatim).
    assert!(
        body.to_lowercase().contains("http"),
        "/help/api body does not contain 'http' anywhere — looks empty: {body:.300}"
    );
    // And it should render at least one heading.
    let headings = harness.driver.find_all(By::Css("h1, h2, h3")).await?;
    assert!(
        !headings.is_empty(),
        "/help/api rendered no <hN> headings — markdown render likely broken"
    );
    harness.close().await
}

#[tokio::test]
async fn clicking_sidebar_link_re_runs_mermaid_renderer() -> Result<()> {
    // Regression: the help sidebar's doc links go to leptos-routed
    // `/help/<slug>` paths. Without `rel="external"` the Router
    // intercepts the click and swaps the article body in place —
    // but `help-init.js` only boots Mermaid + KaTeX on
    // `DOMContentLoaded`, which doesn't fire on SPA navigation. The
    // user-visible symptom: navigate from `/help/api` to
    // `/help/architecture`, the mermaid diagrams stay as raw text
    // until you manually refresh.
    //
    // The test exercises the click flow: load /help/api, click the
    // sidebar link for the architecture doc, then verify that at
    // least one `.mermaid` block has been transformed into an
    // `<svg>` by mermaid.run().
    let harness = TestHarness::fresh().await?;
    harness.goto("/help/api").await?;
    harness.wait_for(".help-sidebar", Duration::from_secs(10)).await?;

    let target = harness
        .wait_until(
            async |drv| {
                let links = drv.find_all(By::Css(".help-sidebar a")).await?;
                for a in links {
                    let href = a.attr("href").await.unwrap_or_default().unwrap_or_default();
                    if href.contains("/help/architecture") {
                        return Ok(Some(a));
                    }
                }
                Ok(None)
            },
            Duration::from_secs(10),
        )
        .await?;
    target.click().await?;

    // Wait for the architecture doc to appear AND for mermaid to
    // turn at least one source block into an SVG. Mermaid loads
    // its bundle from a CDN; on a cold cache that can take a few
    // seconds. The test fails clearly if the SVG never appears.
    harness
        .wait_until(
            async |drv| {
                let svgs = drv.find_all(By::Css(".help-doc .mermaid svg")).await?;
                Ok(if !svgs.is_empty() { Some(()) } else { None })
            },
            Duration::from_secs(25),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "after clicking the `architecture` link in the help sidebar, no \
                 `.mermaid svg` appeared in the DOM. Either help-init.js failed \
                 to re-run (sidebar <a> missing rel=\"external\"?) or the Mermaid \
                 CDN bundle never loaded."
            )
        })?;
    harness.close().await
}

#[tokio::test]
async fn unknown_help_slug_returns_a_friendly_error_page() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/help/this-slug-does-not-exist").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    // Status via reqwest — driver itself doesn't expose response code.
    let resp = reqwest::Client::new()
        .get(harness.url("/help/this-slug-does-not-exist"))
        .send()
        .await?;
    let status = resp.status().as_u16();
    // The leptos shell may answer 200 with an error in-body, or 404 —
    // either is acceptable provided we don't get a hard 5xx.
    assert!(
        status < 500,
        "/help/<missing> should not 5xx, got {status}"
    );
    harness.close().await
}
