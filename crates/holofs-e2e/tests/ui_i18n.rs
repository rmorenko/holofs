//! i18n: the topbar locale switcher cycles 5 languages and an
//! unknown `?lang=` falls back to English. Server-rendered, no
//! hydration involved.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn each_locale_renders_its_translation_for_catalog() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    // (?lang code, marker word that MUST appear in that locale's body)
    let cases = [
        ("en", "catalog"),
        ("ru", "каталог"),
        ("de", "Katalog"),
        ("fr", "catalogue"),
        ("es", "catálogo"),
    ];
    for (lang, word) in cases {
        harness.goto(&format!("/?lang={lang}")).await?;
        harness.wait_for("body", Duration::from_secs(5)).await?;
        let body = harness.driver.find(By::Css("body")).await?.text().await?;
        assert!(
            body.to_lowercase().contains(&word.to_lowercase()),
            "?lang={lang}: expected marker word '{word}' in body, got: {body:.200}"
        );
    }
    harness.close().await
}

#[tokio::test]
async fn unknown_locale_falls_back_to_english() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/?lang=ja").await?;
    harness.wait_for("body", Duration::from_secs(5)).await?;
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    // Some English-only word that wouldn't survive translation. The
    // catalog page topbar shows "catalog" in en; "каталог" in ru;
    // "Katalog" in de — `?lang=ja` should fall back to en.
    assert!(
        body.to_lowercase().contains("catalog"),
        "expected `catalog` (en) on ?lang=ja fallback page; got: {body:.200}"
    );
    harness.close().await
}
