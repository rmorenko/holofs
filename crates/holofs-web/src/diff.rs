//! `/diff/:a/:b` — per-chunk colored grid showing which systematic shards
//! are shared (green) vs different (red) between two same-kind objects.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use serde::{Deserialize, Serialize};

use crate::t;

// ===== View-models (Send across SSR ⇄ hydrate boundary) ====================

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffCellView {
    pub idx: u32,
    pub is_common: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffLayerView {
    pub channel: u8,
    pub layer: u8,
    pub n_common: usize,
    pub n_total: usize,
    pub cells: Vec<DiffCellView>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiffReportView {
    pub name_a: String,
    pub name_b: String,
    pub kind: String,
    pub common: usize,
    pub total: usize,
    pub similarity_pct: f32,
    pub storage_saved_kb: u64,
    pub layers: Vec<DiffLayerView>,
}

// ===== Server function =====================================================

#[server(
    name = GetDiff,
    prefix = "/api",
    endpoint = "diff",
)]
pub async fn get_diff(a: String, b: String) -> Result<DiffReportView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let rep = gw
        .diff_chunks(&a, &b)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(report_to_view(rep))
}

#[cfg(feature = "ssr")]
fn report_to_view(rep: holofs_gateway::DiffReport) -> DiffReportView {
    use holofs_model::manifest::ObjectKind;
    let kind = match rep.kind {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
    .to_string();
    DiffReportView {
        name_a: rep.name_a,
        name_b: rep.name_b,
        kind,
        common: rep.common,
        total: rep.total,
        similarity_pct: rep.similarity_pct,
        storage_saved_kb: rep.storage_saved_bytes / 1024,
        layers: rep
            .layers
            .into_iter()
            .map(|ly| DiffLayerView {
                channel: ly.channel,
                layer: ly.layer,
                n_common: ly.n_common,
                n_total: ly.n_total,
                cells: ly
                    .cells
                    .into_iter()
                    .map(|c| DiffCellView {
                        idx: c.idx,
                        is_common: c.is_common,
                    })
                    .collect(),
            })
            .collect(),
    }
}

// ===== Components ==========================================================

#[component]
pub fn DiffPage() -> impl IntoView {
    // Stage 9: two object paths don't fit a single routable pattern, so the
    // catalog paths are passed as `?a=…&b=…` query string instead of route
    // segments. Both must be present; anything missing is treated as a
    // 400-equivalent in the view layer.
    let query = use_query_map();
    let names = move || {
        query.with(|p| {
            (
                p.get("a").unwrap_or_default(),
                p.get("b").unwrap_or_default(),
            )
        })
    };

    let data = Resource::new(names, |(a, b)| async move {
        if a.is_empty() || b.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing query params 'a' and 'b'".into(),
            ))
        } else {
            get_diff(a, b).await
        }
    });

    view! {
        <crate::ui::Topbar active="catalog"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("diff.loading")}</p> }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <DiffBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn DiffBody(data: DiffReportView) -> impl IntoView {
    let DiffReportView {
        name_a,
        name_b,
        kind,
        common,
        total,
        similarity_pct,
        storage_saved_kb,
        layers,
    } = data;
    let enc_a = crate::url_encode(&name_a);
    let enc_b = crate::url_encode(&name_b);
    let footer_enc_a = enc_a.clone();
    let is_text = kind == "text";
    let sim = format!("{similarity_pct:.1}%");
    let common_str = format!("{common}/{total}");
    let kb = format!("~{storage_saved_kb} KB");

    view! {
        <h2 style="margin-top:0">
            {t!("diff.title_prefix")} " "
            <a href={format!("/{enc_a}")}>{name_a.clone()}</a>
            " ↔ "
            <a href={format!("/{enc_b}")}>{name_b.clone()}</a>
        </h2>
        <p class="mut">{t!("diff.intro")}</p>
        <p class="mut">{t!("diff.not_visual_warn")}</p>
        <p class="mut">{t!("diff.useful_blurb")}</p>
        {is_text.then(|| view! {
            <p class="mut">{t!("diff.text_limit_warn")}</p>
        })}

        <div class="cluster-stats">
            <div class="stat"><div class="v">{common_str}</div><div class="l">{t!("diff.stat.identical")}</div></div>
            <div class="stat"><div class="v">{sim}</div><div class="l">{t!("diff.stat.byte_overlap")}</div></div>
            <div class="stat"><div class="v">{kb}</div><div class="l">{t!("diff.stat.storage_saved")}</div></div>
        </div>

        {layers.into_iter().map(move |ly| {
            let kind_label = if is_text { t!("diff.kind.text_chunks") } else { t!("diff.kind.systematic_shards") };
            view! {
                <h4 class="inspect-layer">
                    "ch" {ly.channel} " · L" {ly.layer} " — "
                    {ly.n_common} "/" {ly.n_total} " " {kind_label}
                </h4>
                <div class="diff-grid">
                    {ly.cells.into_iter().map(|c| {
                        let cls = if c.is_common { "diff-cell common" } else { "diff-cell diff" };
                        let title = format!("chunk #{}", c.idx);
                        view! {
                            <div class=cls title=title>{c.idx}</div>
                        }
                    }).collect_view()}
                </div>
            }
        }).collect_view()}

        <p class="mut">
            {t!("diff.storage_explainer_prefix")} " "
            <b>{common}</b>
            " " {t!("diff.storage_explainer_suffix")}
        </p>

        <p style="margin-top:24px">
            <a href={format!("/similar/{footer_enc_a}")}>{t!("diff.footer.similar_a")}</a>
            " · "
            <a href="/">{t!("generic.back_to_catalog")}</a>
        </p>
    }
}
