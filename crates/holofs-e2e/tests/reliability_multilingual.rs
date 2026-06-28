//! Regression guard for the multilingual text encoder swap
//! (commit `550b687`). Pre-swap the gateway used the original
//! English-only CLIP text branch; non-English queries returned
//! ~3% scoring noise with no semantic alignment. Post-swap the
//! distilbert-multilingual-cased + 768→512 projection lands in
//! the same 512-d image space, so a query like "сиреневый круг
//! в центре" surfaces actual circular content.
//!
//! Both tests are `#[ignore]`'d because the embeddings cache
//! requires ~700 MiB of model weights on a cold HuggingFace cache.
//! Opt in via `--include-ignored`.

use anyhow::Result;
use holofs_e2e::{HarnessConfig, TestHarness};

fn embedding_config() -> HarnessConfig {
    HarnessConfig {
        enable_embed: true,
        ..HarnessConfig::default()
    }
}

#[tokio::test]
#[ignore = "downloads ~700 MiB of model weights on first run; opt in via --include-ignored"]
async fn russian_query_against_seeded_corpus_returns_sane_ranking() -> Result<()> {
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    harness.seed_for_search().await?;
    let _ = harness.post_form("/api/embed_all", &[]).await;

    // "снежные горы" url-encoded — exercise the multilingual text
    // branch on a query that should match the snowy/forest/mountain
    // synthetic corpus.
    let r = harness
        .get_json("/api/search?q=%D1%81%D0%BD%D0%B5%D0%B6%D0%BD%D1%8B%D0%B5+%D0%B3%D0%BE%D1%80%D1%8B&band=any&limit=5")
        .await?;
    let hits = r
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !hits.is_empty(),
        "Russian semantic-search query returned 0 hits — multilingual encoder dead?"
    );

    // Pre-swap signature of the noise mode: every hit's score sat
    // within ~3% of the next. A working multilingual encoder gives
    // a spread of at least 0.02 between the top and the bottom of
    // a 5-deep ranking.
    let scores: Vec<f64> = hits
        .iter()
        .filter_map(|h| h.get("score").and_then(|s| s.as_f64()))
        .collect();
    if scores.len() >= 2 {
        let max = scores.iter().cloned().fold(f64::MIN, f64::max);
        let min = scores.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            (max - min) > 0.02,
            "top-5 score spread = {:.4} (max={max:.4} min={min:.4}) — \
             pre-fix noise band. The CLIP text encoder is back, or the \
             multilingual projection got reverted.",
            max - min
        );
    }
    harness.close().await
}

#[tokio::test]
#[ignore = "downloads ~700 MiB of model weights on first run; opt in via --include-ignored"]
async fn ru_and_en_queries_overlap_on_the_same_concept() -> Result<()> {
    // Cross-language alignment: the same concept in two languages
    // should pick the same images. Pre-fix the Russian ranking was
    // essentially random and overlap was zero by chance.
    let harness = TestHarness::fresh_with(embedding_config()).await?;
    harness.seed_for_search().await?;
    let _ = harness.post_form("/api/embed_all", &[]).await;

    let names = |resp: &serde_json::Value| -> std::collections::HashSet<String> {
        resp.get("hits")
            .and_then(|h| h.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| h.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };

    let ru = harness
        .get_json("/api/search?q=%D1%81%D0%BD%D0%B5%D0%B6%D0%BD%D1%8B%D0%B5+%D0%B3%D0%BE%D1%80%D1%8B&band=any&limit=5")
        .await?;
    let en = harness
        .get_json("/api/search?q=snowy+mountains&band=any&limit=5")
        .await?;

    let ru_n = names(&ru);
    let en_n = names(&en);
    let overlap = ru_n.intersection(&en_n).count();
    assert!(
        overlap >= 2,
        "ru and en queries for the same concept overlap on only {overlap}/5 hits; \
         ru={ru_n:?} en={en_n:?} — multilingual projection may be regressed"
    );
    harness.close().await
}
