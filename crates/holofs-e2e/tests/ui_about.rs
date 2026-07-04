//! `/about` marketing page.
//!
//! Lightweight rendering checks — the page is static SSR with no
//! hydration needs, so this also serves as the simplest possible
//! sanity test for the harness itself.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn about_page_returns_200_and_mentions_holofs() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/about").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    assert!(
        body.to_lowercase().contains("holofs"),
        "/about page does not mention 'holofs': {body:.300}"
    );
    harness.close().await
}

#[tokio::test]
async fn about_page_has_topbar_link_back_to_catalog() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/about").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    // Find any link whose href targets the root catalog. The page
    // template's topbar renders a "catalog" / "каталог" / "Katalog" /
    // "catalogue" / "catálogo" link regardless of the active locale.
    let links = harness
        .driver
        .find_all(By::Css("a[href]"))
        .await?;
    let mut found_root = false;
    for a in links {
        if let Some(href) = a.attr("href").await? {
            if href == "/" || href.ends_with("//") {
                found_root = true;
                break;
            }
        }
    }
    assert!(found_root, "/about has no <a href=\"/\"> link back to the catalog");
    harness.close().await
}
