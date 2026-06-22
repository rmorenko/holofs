//! `/similar/:name` — top-10 neighbours by perceptual fingerprint (image/audio)
//! or MinHash Jaccard (text) plus cross-object shard-hash overlaps.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

// ===== View-models (Send across SSR ⇄ hydrate boundary) ====================

/// Comparison method used to score a neighbour.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MethodView {
    /// MinHash Jaccard over 5-shingles (text).
    Jaccard,
    /// L1 distance over the 16-byte perceptual fingerprint (image/audio).
    L1,
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
pub async fn get_similar(name: String) -> Result<SimilarReportView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let rep = gw
        .similar_to(&name)
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
                    SimilarityMethod::L1 => MethodView::L1,
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
    let name = move || params.with(|p| p.get("name").unwrap_or_default());

    let data = Resource::new(name, |n| async move {
        if n.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing name".into(),
            ))
        } else {
            get_similar(n).await
        }
    });

    view! {
        <crate::ui::Topbar active="catalog"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">"loading similar…"</p> }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <SimilarBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">"failed to load: " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn SimilarBody(data: SimilarReportView) -> impl IntoView {
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

    view! {
        <h2 style="margin-top:0">
            "find similar to "
            <a href={format!("/{object_link_enc}")}>{name.clone()}</a>
        </h2>

        {if is_text {
            view! {
                <p class="mut">
                    "method: " <b>"bottom-K MinHash on 5-shingles"</b> ", K=" {minhash_k}
                    ". fingerprint (first 8 values): " <code>{fingerprint_hex.clone()}</code>
                </p>
                <p class="mut">
                    "MinHash catches " <b>"partial text overlaps"</b>
                    ": if 60% of document A is contained in document B, Jaccard surfaces it "
                    "even when a plain SHA-256 wouldn't match. Useful for plagiarism "
                    "detection, finding drafts, dedup of edited texts."
                </p>
            }.into_any()
        } else {
            view! {
                <p class="mut">
                    "method: " <b>"perceptual fingerprint on L0 shards"</b>
                    ". fingerprint (16 bytes): " <code>{fingerprint_hex.clone()}</code>
                </p>
                <p class="mut">
                    "fingerprint is computed on K=16 systematic L0 shards (DWT LL band for "
                    "image, bass envelope for audio) — \"low resolution in the frequency "
                    "domain\", analogous to dHash. L1 distance over u8."
                </p>
            }.into_any()
        }}

        <h3>"top similar (" {n_neighbors} ")"</h3>
        {if neighbors.is_empty() {
            view! { <p class="mut">"no other objects of the same kind in the catalog"</p> }.into_any()
        } else {
            let self_enc = self_enc.clone();
            view! {
                <table>
                    <tr>
                        <th class="name">"name"</th>
                        <th>"similarity"</th>
                        <th>"method"</th>
                        <th>"actions"</th>
                    </tr>
                    {neighbors.into_iter().map(|m| {
                        let cls = if m.similarity_pct > 90.0 { "ok" }
                                  else if m.similarity_pct > 50.0 { "partial" }
                                  else { "mut" };
                        let method = match m.method {
                            MethodView::Jaccard => "Jaccard",
                            MethodView::L1 => "L1",
                        };
                        let nenc = crate::url_encode(&m.name);
                        let sim = format!("{:.1}%", m.similarity_pct);
                        let self_enc = self_enc.clone();
                        view! {
                            <tr>
                                <td class="name">
                                    <a href={format!("/similar/{nenc}")}>{m.name.clone()}</a>
                                </td>
                                <td class=cls><b>{sim}</b></td>
                                <td class="name"><code>{method}</code></td>
                                <td class="name">
                                    <a href={format!("/{nenc}")} target="_blank">"open"</a>
                                    " · "
                                    <a href={format!("/inspect/{nenc}")}>"shards"</a>
                                    " · "
                                    <a href={format!("/diff?a={self_enc}&b={nenc}")}>"diff →"</a>
                                </td>
                            </tr>
                        }
                    }).collect_view()}
                </table>
            }.into_any()
        }}

        <h3>"shard overlaps (" {n_overlaps} ")"</h3>
        {if overlaps.is_empty() {
            view! {
                <p class="mut">
                    "unique object — no shard hash overlaps with any other"
                </p>
            }.into_any()
        } else {
            view! {
                <p class="mut">
                    "dedup at the SHA-256 shard level. An overlap means that part of the data "
                    "is already physically stored in the cluster (new shards do not duplicate it)."
                </p>
                <table>
                    <tr>
                        <th class="name">"name"</th>
                        <th>"common shards"</th>
                        <th>"% overlap"</th>
                    </tr>
                    {overlaps.into_iter().map(|o| {
                        let nenc = crate::url_encode(&o.name);
                        let pct = format!("{:.1}%", o.overlap_pct);
                        view! {
                            <tr>
                                <td class="name">
                                    <a href={format!("/similar/{nenc}")}>{o.name.clone()}</a>
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
            <a href={format!("/inspect/{footer_enc}")}>"← all shards"</a>
            " · "
            <a href="/">"catalog"</a>
        </p>
    }
}
