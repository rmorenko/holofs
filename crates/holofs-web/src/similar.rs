//! `/similar/:name` — top-10 neighbours by perceptual fingerprint (image/audio)
//! or MinHash Jaccard (text) plus cross-object shard-hash overlaps.

use leptos::prelude::*;
use leptos_router::hooks::{use_params_map, use_query_map};
use serde::{Deserialize, Serialize};

use crate::i18n::{current_locale, translate};
use crate::t;

// ===== View-models (Send across SSR ⇄ hydrate boundary) ====================

/// Comparison method used to score a neighbour.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MethodView {
    /// MinHash Jaccard over 5-shingles (text).
    Jaccard,
    /// Hamming distance over the 45-bit dHash derived from the per-channel
    /// 48-byte fingerprint (image/audio). Stage 11.6 retired the old
    /// `L1` byte-magnitude scoring after it kept saturating at 95%+ on
    /// unrelated photos.
    DHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SimilarMatchView {
    pub name: String,
    pub similarity_pct: f32,
    pub method: MethodView,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShardOverlapView {
    pub name: String,
    pub common: usize,
    pub overlap_pct: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SimilarReportView {
    pub name: String,
    pub kind: String,
    pub fingerprint_hex: String,
    pub minhash_k: usize,
    pub total_shards: usize,
    pub neighbors: Vec<SimilarMatchView>,
    pub overlaps: Vec<ShardOverlapView>,
}

// ===== Server functions ====================================================

#[server(
    name = GetSimilar,
    prefix = "/api",
    endpoint = "similar",
)]
pub async fn get_similar(
    name: String,
    scope: String,
) -> Result<SimilarReportView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let scope = holofs_gateway::SimilarScope::parse(&scope);
    let rep = gw
        .similar_to(&name, scope)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(report_to_view(rep))
}

#[cfg(feature = "ssr")]
fn report_to_view(rep: holofs_gateway::SimilarReport) -> SimilarReportView {
    use holofs_gateway::SimilarityMethod;
    use holofs_model::manifest::ObjectKind;
    let kind = match rep.kind {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
    .to_string();
    SimilarReportView {
        name: rep.name,
        kind,
        fingerprint_hex: rep.fingerprint_hex,
        minhash_k: rep.minhash_k,
        total_shards: rep.total_shards,
        neighbors: rep
            .neighbors
            .into_iter()
            .map(|m| SimilarMatchView {
                name: m.name,
                similarity_pct: m.similarity_pct,
                method: match m.method {
                    SimilarityMethod::Jaccard => MethodView::Jaccard,
                    SimilarityMethod::DHash => MethodView::DHash,
                },
            })
            .collect(),
        overlaps: rep
            .overlaps
            .into_iter()
            .map(|o| ShardOverlapView {
                name: o.name,
                common: o.common,
                overlap_pct: o.overlap_pct,
            })
            .collect(),
    }
}

// ===== Components ==========================================================

#[component]
pub fn SimilarPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let name = move || params.with(|p| p.get("name").unwrap_or_default());
    // Stage 11.16: `?scope=all|folder|tree`. Default `all` keeps the
    // existing behaviour for bookmarks made before this stage.
    let scope = move || {
        query
            .with(|q| q.get("scope").unwrap_or_default())
            .to_string()
    };

    let inputs = move || (name(), scope());
    let data = Resource::new(inputs, |(n, s)| async move {
        if n.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing name".into(),
            ))
        } else {
            get_similar(n, s).await
        }
    });

    view! {
        <crate::ui::Topbar active="catalog"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("similar.loading")}</p> }>
                {move || data.get().map(|res| {
                    let active_scope = scope();
                    match res {
                        Ok(v) => view! { <SimilarBody data=v active_scope=active_scope/> }.into_any(),
                        Err(e) => view! {
                            <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                        }.into_any(),
                    }
                })}
            </Suspense>
        </main>
    }
}

/// Build a `/similar/<name>` URL preserving the current `?scope=` and
/// `?lang=`. Empty values are dropped so the URL stays clean.
fn similar_href(name_enc: &str, scope: &str) -> String {
    let lang = current_locale();
    let mut params: Vec<String> = Vec::new();
    if !scope.is_empty() && scope != "all" {
        params.push(format!("scope={scope}"));
    }
    if lang != "en" {
        params.push(format!("lang={lang}"));
    }
    if params.is_empty() {
        format!("/similar/{name_enc}")
    } else {
        format!("/similar/{name_enc}?{}", params.join("&"))
    }
}

#[component]
fn SimilarBody(data: SimilarReportView, active_scope: String) -> impl IntoView {
    let SimilarReportView {
        name,
        kind,
        fingerprint_hex,
        minhash_k,
        total_shards: _,
        neighbors,
        overlaps,
    } = data;
    let enc = crate::url_encode(&name);
    let is_text = kind == "text";
    let n_neighbors = neighbors.len();
    let n_overlaps = overlaps.len();
    let self_enc = enc.clone();
    let footer_enc = enc.clone();
    let object_link_enc = enc.clone();
    let scope_for_links = if active_scope.is_empty() {
        "all".to_string()
    } else {
        active_scope.clone()
    };

    view! {
        <h2 style="margin-top:0">
            {t!("similar.title_prefix")} " "
            <a href={format!("/{object_link_enc}")}>{name.clone()}</a>
        </h2>

        <ScopeSelector name_enc=enc.clone() active=scope_for_links.clone()/>

        {if is_text {
            view! {
                <p class="mut">
                    {t!("similar.method_label")} " "
                    <b>{t!("similar.method.minhash_name")}</b>
                    ", K=" {minhash_k}
                    {t!("similar.method.minhash_fp")} " "
                    <code>{fingerprint_hex.clone()}</code>
                </p>
                <p class="mut">{t!("similar.method.minhash_blurb")}</p>
            }.into_any()
        } else {
            view! {
                <p class="mut">
                    {t!("similar.method_label")} " "
                    <b>{t!("similar.method.dhash_name")}</b>
                    {t!("similar.method.dhash_fp")} " "
                    <code>{fingerprint_hex.clone()}</code>
                </p>
                <p class="mut">{t!("similar.method.dhash_blurb")}</p>
            }.into_any()
        }}

        <h3>{t!("similar.top_h")} " (" {n_neighbors} ")"</h3>
        {if neighbors.is_empty() {
            view! { <p class="mut">{t!("similar.top_empty")}</p> }.into_any()
        } else {
            let self_enc = self_enc.clone();
            view! {
                <table>
                    <tr>
                        <th class="name">{t!("similar.col.name")}</th>
                        <th>{t!("similar.col.similarity")}</th>
                        <th>{t!("similar.col.method")}</th>
                        <th>{t!("similar.col.actions")}</th>
                    </tr>
                    {neighbors.into_iter().map(|m| {
                        let cls = if m.similarity_pct > 90.0 { "ok" }
                                  else if m.similarity_pct > 50.0 { "partial" }
                                  else { "mut" };
                        let method = match m.method {
                            MethodView::Jaccard => "Jaccard",
                            MethodView::DHash => "dHash",
                        };
                        let nenc = crate::url_encode(&m.name);
                        let sim = format!("{:.1}%", m.similarity_pct);
                        let self_enc = self_enc.clone();
                        let href = similar_href(&nenc, &scope_for_links);
                        view! {
                            <tr>
                                <td class="name">
                                    <a href=href>{m.name.clone()}</a>
                                </td>
                                <td class=cls><b>{sim}</b></td>
                                <td class="name"><code>{method}</code></td>
                                <td class="name">
                                    <a href={format!("/{nenc}")} target="_blank">{t!("similar.action.open")}</a>
                                    " · "
                                    <a href={format!("/inspect/{nenc}")}>{t!("similar.action.shards")}</a>
                                    " · "
                                    <a href={format!("/diff?a={self_enc}&b={nenc}")}>{t!("similar.action.diff")}</a>
                                </td>
                            </tr>
                        }
                    }).collect_view()}
                </table>
            }.into_any()
        }}

        <h3>{t!("similar.overlaps_h")} " (" {n_overlaps} ")"</h3>
        {if overlaps.is_empty() {
            view! {
                <p class="mut">{t!("similar.overlaps_empty")}</p>
            }.into_any()
        } else {
            view! {
                <p class="mut">{t!("similar.overlaps_blurb")}</p>
                <table>
                    <tr>
                        <th class="name">{t!("similar.col.name")}</th>
                        <th>{t!("similar.col.common")}</th>
                        <th>{t!("similar.col.overlap_pct")}</th>
                    </tr>
                    {overlaps.into_iter().map(|o| {
                        let nenc = crate::url_encode(&o.name);
                        let pct = format!("{:.1}%", o.overlap_pct);
                        let href = similar_href(&nenc, &scope_for_links);
                        view! {
                            <tr>
                                <td class="name">
                                    <a href=href>{o.name.clone()}</a>
                                </td>
                                <td><code>{o.common}</code></td>
                                <td><b>{pct}</b></td>
                            </tr>
                        }
                    }).collect_view()}
                </table>
            }.into_any()
        }}

        <p style="margin-top:24px">
            <a href={format!("/inspect/{footer_enc}")}>{t!("similar.footer.all_shards")}</a>
            " · "
            <a href="/">{t!("generic.back_to_catalog")}</a>
        </p>
    }
}

/// Stage 11.16: scope picker for `/similar/<name>`. Three modes —
/// whole catalog (legacy), direct siblings, subtree. SSR-friendly: each
/// option is an `<a>` so the browser reloads with the new `?scope=`.
#[component]
fn ScopeSelector(name_enc: String, active: String) -> impl IntoView {
    let options = [
        ("all", "similar.scope.all"),
        ("folder", "similar.scope.folder"),
        ("tree", "similar.scope.tree"),
    ];
    let items = options.iter().map(|(slug, key)| {
        let slug = *slug;
        let key = *key;
        let is_active = slug == active;
        let href = similar_href(&name_enc, slug);
        let label = translate(key, &current_locale());
        if is_active {
            view! {
                <span class="scope-pill scope-active">{label}</span>
            }.into_any()
        } else {
            view! {
                <a class="scope-pill" href=href>{label}</a>
            }.into_any()
        }
    }).collect_view();
    view! {
        <p class="scope-picker mut">
            <span class="scope-label">{move || translate("similar.scope.label", &current_locale())}</span>
            {items}
        </p>
    }
}
