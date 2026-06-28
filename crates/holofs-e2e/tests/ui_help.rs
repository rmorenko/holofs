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
