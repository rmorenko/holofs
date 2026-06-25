//! `/search` — natural-language semantic search over the catalog.
//!
//! Single text input, single results grid. Each card initially renders
//! its coarse-layer preview (`/preview/<name>`, ~8% of the bytes) and
//! swaps to the full image (`/<name>`) once the latter finishes loading
//! — the user perceives the gallery *focusing* as detail layers stream
//! in over the wire. That's the architectural pitch from `/about` made
//! visible.
//!
//! The 2-pass re-ranking (Coarse band → Mid band refinement) lives on
//! the roadmap for Stage 13.3 (hierarchical index by layer level). For
//! now Stage 12.9 is a single-pass query against the existing Coarse
//! band index.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use serde::{Deserialize, Serialize};

use crate::i18n::current_locale;
use crate::t;

// ===== View-model (Send across SSR ⇄ hydrate) ==============================

/// One row in [`SearchResultsView::hits`]. Kept tiny — the page reads
/// the catalog separately to dress each name with kind / dimensions
/// when needed (the gateway returns the score and that's it).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SearchHitView {
    /// Catalog name of the matching object.
    pub name: String,
    /// Cosine similarity, raw float. CLIP-base scores cluster around
    /// 0.2-0.35 for strong matches; the UI scales them into a 0..100
    /// confidence bar.
    pub score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchResultsView {
    pub query: String,
    pub hits: Vec<SearchHitView>,
    /// `true` when the gateway has the embed feature on. Cleared when
    /// the server was started without `--enable-embed` — the UI
    /// renders an explanatory empty state instead of pretending no
    /// images matched.
    pub embed_enabled: bool,
}

// ===== Server function =====================================================

#[server(
    name = SemanticSearch,
    prefix = "/api",
    endpoint = "search_view",
)]
pub async fn semantic_search(query: String) -> Result<SearchResultsView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let enabled = gw.embed_enabled().await;
    if !enabled || query.trim().is_empty() {
        return Ok(SearchResultsView {
            query,
            hits: Vec::new(),
            embed_enabled: enabled,
        });
    }
    let raw = gw
        .semantic_search(&query, 50)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(SearchResultsView {
        query,
        hits: raw
            .into_iter()
            .map(|h| SearchHitView {
                name: h.name,
                score: h.score,
            })
            .collect(),
        embed_enabled: true,
    })
}

// ===== Components ==========================================================

/// `GET /search` — query input + ranked results grid.
#[component]
pub fn SearchPage() -> impl IntoView {
    let query = use_query_map();
    let q = move || query.with(|q| q.get("q").unwrap_or_default());

    let data = Resource::new(q, |query| async move {
        semantic_search(query).await
    });

    view! {
        <crate::ui::Topbar active="search"/>
        <main class="container">
            <h2 class="search-h">{t!("search.title")}</h2>
            <p class="mut search-intro">{t!("search.intro")}</p>

            <SearchForm initial=q()/>

            <Suspense fallback=move || view! {
                <p class="mut">{t!("search.loading")}</p>
            }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <SearchBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn SearchForm(initial: String) -> impl IntoView {
    let lang_hidden = {
        let lang = current_locale();
        (lang != "en").then_some(lang)
    };
    view! {
        <form class="search-form" method="GET" action="/search">
            <input
                type="text"
                name="q"
                class="search-input"
                value=initial
                placeholder=t!("search.placeholder")
                autocomplete="off"
                autofocus=true
            />
            {lang_hidden.map(|l| view! {
                <input type="hidden" name="lang" value=l/>
            })}
            <button type="submit" class="tree-control-btn search-submit">
                {t!("search.submit")}
            </button>
        </form>
    }
}

#[component]
fn SearchBody(data: SearchResultsView) -> impl IntoView {
    let SearchResultsView {
        query,
        hits,
        embed_enabled,
    } = data;

    if !embed_enabled {
        return view! {
            <p class="bad search-disabled">{t!("search.disabled")}</p>
        }
        .into_any();
    }
    if query.trim().is_empty() {
        return view! {
            <p class="mut search-empty">{t!("search.empty_prompt")}</p>
        }
        .into_any();
    }
    if hits.is_empty() {
        return view! {
            <p class="mut search-empty">{t!("search.no_hits")}</p>
        }
        .into_any();
    }
    let max_score = hits
        .iter()
        .map(|h| h.score)
        .fold(0.0_f32, f32::max)
        .max(1e-6);
    let n_hits = hits.len();

    view! {
        <p class="mut search-count">
            {n_hits.to_string()} " " {t!("search.results_for")} " "
            <b>"\"" {query.clone()} "\""</b>
        </p>
        <div class="search-grid">
            {hits.into_iter().map(|h| {
                let enc = crate::url_encode(&h.name);
                let coarse_src = format!("/preview/{enc}");
                let full_src = format!("/{enc}");
                let alt = h.name.clone();
                let pct = (h.score / max_score * 100.0).clamp(0.0, 100.0);
                let raw_score = format!("{:.3}", h.score);
                view! {
                    <a class="search-card" href={format!("/{enc}")} rel="external">
                        <div class="search-thumb-wrap">
                            // Stage 12.9: lo-fi preview first paint, then
                            // the browser fetches the full-res image and
                            // swaps when ready. CSS cross-fades the swap
                            // so the user perceives the result
                            // "focusing".
                            <img
                                class="search-thumb-coarse"
                                src=coarse_src
                                alt=alt.clone()
                                loading="lazy"
                            />
                            <img
                                class="search-thumb-full"
                                src=full_src
                                alt=alt
                                loading="lazy"
                            />
                        </div>
                        <div class="search-card-meta">
                            <div class="search-card-name">{h.name}</div>
                            <div class="search-card-score">
                                <div class="search-score-bar">
                                    <div class="search-score-fill" style=format!("width:{pct:.1}%")></div>
                                </div>
                                <code class="mut">{raw_score}</code>
                            </div>
                        </div>
                    </a>
                }
            }).collect_view()}
        </div>
    }
    .into_any()
}
