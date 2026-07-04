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
//! the roadmap for (hierarchical index by layer level). For
//! now is a single-pass query against the existing Coarse
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
    /// layer band that produced the winning score for this
    /// file. UI badges results with it so the user can tell apart "the
    /// match was on silhouette" from "the match was on texture".
    pub band: String,
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
    /// Active band filter (`coarse` / `mid` / `full` / `any`).
    pub band: String,
}

// ===== Server function =====================================================

#[server(
    name = SemanticSearch,
    prefix = "/api",
    endpoint = "search_view",
)]
pub async fn semantic_search(
    query: String,
    band: String,
) -> Result<SearchResultsView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let enabled = gw.embed_enabled().await;
    let parsed_band = holofs_gateway::SearchBand::parse(&band);
    let band_str = match parsed_band {
        holofs_gateway::SearchBand::Coarse => "coarse",
        holofs_gateway::SearchBand::Mid => "mid",
        holofs_gateway::SearchBand::Full => "full",
        holofs_gateway::SearchBand::Any => "any",
    }
    .to_string();
    if !enabled || query.trim().is_empty() {
        return Ok(SearchResultsView {
            query,
            hits: Vec::new(),
            embed_enabled: enabled,
            band: band_str,
        });
    }
    let raw = gw
        .semantic_search(&query, 50, parsed_band)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(SearchResultsView {
        query,
        hits: raw
            .into_iter()
            .map(|h| SearchHitView {
                name: h.name,
                score: h.score,
                band: match h.band {
                    holofs_gateway::SearchBand::Coarse => "coarse".into(),
                    holofs_gateway::SearchBand::Mid => "mid".into(),
                    holofs_gateway::SearchBand::Full => "full".into(),
                    holofs_gateway::SearchBand::Any => "any".into(),
                },
            })
            .collect(),
        embed_enabled: true,
        band: band_str,
    })
}

// ===== Components ==========================================================

/// `GET /search` — query input + ranked results grid.
#[component]
pub fn SearchPage() -> impl IntoView {
    let query = use_query_map();
    let inputs = move || {
        query.with(|q| {
            (
                q.get("q").unwrap_or_default(),
                q.get("band").unwrap_or_else(|| "any".to_string()),
            )
        })
    };

    let data = Resource::new(inputs, |(query, band)| async move {
        semantic_search(query, band).await
    });

    view! {
        <crate::ui::Topbar active="search"/>
        <main class="container">
            <h2 class="search-h">{t!("search.title")}</h2>
            <p class="mut search-intro">{t!("search.intro")}</p>

            {move || {
                let (q, band) = inputs();
                view! {
                    <SearchForm initial=q.clone() band=band.clone()/>
                    <BandPicker active=band q=q.clone()/>
                }
            }}

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
fn SearchForm(initial: String, band: String) -> impl IntoView {
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
            <input type="hidden" name="band" value=band/>
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
fn BandPicker(active: String, q: String) -> impl IntoView {
    let opts = [
        ("any", "search.band.any"),
        ("coarse", "search.band.coarse"),
        ("mid", "search.band.mid"),
        ("full", "search.band.full"),
    ];
    let enc_q = crate::url_encode(&q);
    let pills = opts.iter().map(|(slug, key)| {
        let slug = *slug;
        let label = crate::i18n::translate(key, &crate::i18n::current_locale());
        let is_active = slug == active;
        let href = if q.is_empty() {
            format!("/search?band={slug}")
        } else {
            format!("/search?q={enc_q}&band={slug}")
        };
        if is_active {
            view! { <span class="scope-pill scope-active">{label}</span> }.into_any()
        } else {
            view! { <a class="scope-pill" href=href rel="external">{label}</a> }.into_any()
        }
    }).collect_view();
    view! {
        <p class="scope-picker mut search-band">
            <span class="scope-label">{t!("search.band_label")}</span>
            {pills}
        </p>
    }
}

#[component]
fn SearchBody(data: SearchResultsView) -> impl IntoView {
    let SearchResultsView {
        query,
        hits,
        embed_enabled,
        band: _,
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
                            // lo-fi preview first paint, then
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
                                <code class={format!("search-band-badge band-{}", h.band)}>{h.band.clone()}</code>
                            </div>
                        </div>
                    </a>
                }
            }).collect_view()}
        </div>
    }
    .into_any()
}
