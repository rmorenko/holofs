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
async fn clicking_locale_switcher_actually_changes_strings() -> Result<()> {
    // Regression: the locale-switcher link must trigger a full
    // server-rendered reload, not a leptos Router intercept.
    // Without `rel="external"` the URL changes to `?lang=ru` but
    // every `t!()` call was already resolved at SSR time and the
    // visible strings stay in English until the user refreshes.
    // The same kind of regression we already guard against on the
    // topbar nav (feedback memory rule 3).
    //
    // Critical: do this check on `/health/<name>` specifically.
    // It's a page with a lot of translated labels, so any failure
    // to re-render shows up loud and clear in the body text.
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    harness
        .goto("/health/photos/abstract/mandala.png")
        .await?;
    harness.wait_for("body", Duration::from_secs(10)).await?;
    // The locale-switcher renders short locale codes (en / ru / de /
    // fr / es) as `<a>` text. The header containing it can take a
    // beat to mount on heavier pages like /health/<name>; give it
    // enough time and look for the `ru` link by href contents
    // rather than visible text (some Chrome configs return empty
    // `text()` for inline anchors until first layout pass).
    let ru_link = harness
        .wait_until(
            async |drv| {
                let links = drv.find_all(By::Css(".locale-switcher a")).await?;
                for a in links {
                    let href = a.attr("href").await.unwrap_or_default().unwrap_or_default();
                    if href.contains("lang=ru") {
                        return Ok(Some(a));
                    }
                }
                Ok(None)
            },
            Duration::from_secs(15),
        )
        .await?;
    ru_link.click().await?;

    // The page should now be re-rendered in Russian. Find a
    // word that only appears in the ru translation set. The
    // topbar's catalog label is "каталог" in ru and "catalog"
    // in en — perfect marker.
    harness
        .wait_until(
            async |drv| {
                let body = drv.find(By::Css("body")).await?.text().await?;
                Ok(if body.to_lowercase().contains("каталог") {
                    Some(())
                } else {
                    None
                })
            },
            Duration::from_secs(10),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "after clicking the `ru` locale link on /health/<name>, the body \
                 still does not contain the Russian word `каталог`. The link likely \
                 fired a SPA-router navigation instead of a full SSR reload — check \
                 that LocaleSwitcher's <a> carries rel=\"external\"."
            )
        })?;
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
