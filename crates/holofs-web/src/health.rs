//! `/health` and `/health/:name` — Leptos pages for cluster + per-object health.
//!
//! Both pages call server functions that wrap `Gateway::health_index_data`
//! and `Gateway::object_health`. Heavy work (Monte Carlo loss simulation,
//! per-layer shard polling) runs server-side; the result is a serializable
//! view-model that crosses the wire to the rendered HTML (and, once
//! hydration is built, to the WASM bundle on the client).

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::t;

// ===== View-models (Send across SSR ⇄ hydrate boundary) ====================

/// One row of the per-node table on `/health`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeRow {
    pub idx: usize,
    pub addr: String,
    pub zone: u8,
    pub admin_killed: bool,
}

/// Lightweight snapshot pushed over SSE by `GET /api/health/events`. The
/// page initially renders with the full [`HealthIndex`]; the WASM hydrate
/// then subscribes to the SSE stream and patches in fresh snapshots every
/// few seconds without a full reload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub n_live: usize,
    pub n_total: usize,
    pub objects: usize,
    /// Unix-millis timestamp the snapshot was produced server-side.
    pub ts_ms: u64,
}

/// Data backing the `/health` index page.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthIndex {
    pub nodes: Vec<NodeRow>,
    pub objects: Vec<String>,
    pub n_live: usize,
    pub n_total: usize,
}

/// Per (channel, layer) row on `/health/:name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayerRow {
    pub channel: u8,
    pub layer: u8,
    pub n_shards: u32,
    pub n_alive: u32,
    pub n_hosting_nodes: u32,
    pub k: u16,
    pub margin: i32,
}

/// One Monte-Carlo loss scenario row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LossRow {
    /// Percentage of nodes killed at random in this scenario.
    pub kill_pct: u8,
    /// `n_trials` count, propagated so the UI can label the table.
    pub n_trials: u32,
    /// pmf over outcomes: `[dead, L0_only, L0-L1, ..., all_layers]`. Length
    /// is `nlayers + 1`.
    pub pmf: Vec<f32>,
}

/// One "whole zone dies" row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZoneFailureRow {
    pub zone: u8,
    pub nodes_in_zone: u32,
    /// `None` = object dead; `Some(L)` = max surviving layer.
    pub resolution: Option<u8>,
}

/// Backing data for `/health/:name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ObjectHealthView {
    pub name: String,
    pub object_id: u64,
    pub cid_hex: String,
    pub k: u16,
    pub channels: u8,
    pub nlayers: u8,
    pub n_live: usize,
    pub n_total: usize,
    pub current_resolution: Option<u8>,
    pub layers: Vec<LayerRow>,
    pub scenarios: Vec<LossRow>,
    pub zone_failures: Vec<ZoneFailureRow>,
}

/// One row of [`FileMetricsView::neighbours`] — see
/// `holofs_gateway::NeighbourMetric` for the data-side counterpart.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NeighbourRow {
    pub name: String,
    pub kind: String,
    pub shared_total: u64,
    pub shared_per_layer: Vec<u32>,
    pub overlap_pct: f32,
}

/// View-model for the unique per-file metrics block. Renders below the
/// existing margin / Monte Carlo tables on `/health/:name` and is the
/// "what does holofs's architecture buy you" page in miniature.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileMetricsView {
    pub kind: String,
    pub total_shards_in_file: u64,
    pub unique_shards_in_file: u64,
    pub file_dedup_savings_pct: f32,
    pub catalog_total_shards: u64,
    pub catalog_unique_shards: u64,
    pub unique_to_file: u64,
    pub originality_pct: f32,
    pub originality_per_layer: Vec<f32>,
    pub neighbours: Vec<NeighbourRow>,
    pub layer_energy: Option<Vec<f64>>,
    pub audio_bands: Option<(f64, f64, f64)>,
}

// ===== Server functions ====================================================

/// Cluster-wide health snapshot.
#[server(
    name = GetHealthIndex,
    prefix = "/api",
    endpoint = "health_index",
)]
pub async fn get_health_index() -> Result<HealthIndex, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let snap = gw.health_index_data().await;
    Ok(HealthIndex {
        nodes: snap
            .nodes
            .into_iter()
            .map(|n| NodeRow {
                idx: n.idx,
                addr: n.addr,
                zone: n.zone,
                admin_killed: n.admin_killed,
            })
            .collect(),
        objects: snap.objects,
        n_live: snap.n_live,
        n_total: snap.n_total,
    })
}

/// Per-object full health report (margins + Monte Carlo + zone failures).
/// Heavy: polls shard counts per (channel, layer) and runs 5000 trials.
#[server(
    name = GetObjectHealth,
    prefix = "/api",
    endpoint = "object_health",
)]
pub async fn get_object_health(name: String) -> Result<ObjectHealthView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let h = gw
        .object_health(&name)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let cid_hex = h
        .data_cid
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(ObjectHealthView {
        name: h.name,
        object_id: h.object_id,
        cid_hex,
        k: h.k,
        channels: h.channels,
        nlayers: h.nlayers,
        n_live: h.n_live,
        n_total: h.n_nodes,
        current_resolution: h.current_resolution,
        layers: h
            .layers
            .into_iter()
            .map(|l| LayerRow {
                channel: l.channel,
                layer: l.layer,
                n_shards: l.n_shards,
                n_alive: l.n_alive,
                n_hosting_nodes: l.n_hosting_nodes,
                k: l.k,
                margin: l.margin,
            })
            .collect(),
        scenarios: h
            .scenarios
            .into_iter()
            .map(|s| LossRow {
                kill_pct: s.kill_pct,
                n_trials: s.n_trials,
                pmf: s.pmf(),
            })
            .collect(),
        zone_failures: h
            .zone_failures
            .into_iter()
            .map(|z| ZoneFailureRow {
                zone: z.zone,
                nodes_in_zone: z.nodes_in_zone,
                resolution: z.resolution,
            })
            .collect(),
    })
}

/// Per-file unique metrics (originality, dedup, neighbours, layer energy).
/// Heavy: on image/audio this triggers a full DWT decode against the live
/// cluster (one network roundtrip per (channel, layer)). Results stay
/// fresh-on-request; we don't cache because the catalog can change under
/// us between calls.
#[server(
    name = GetFileMetrics,
    prefix = "/api",
    endpoint = "file_metrics",
)]
pub async fn get_file_metrics(name: String) -> Result<FileMetricsView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let m = gw
        .file_metrics(&name)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let kind_str = match m.kind {
        holofs_model::manifest::ObjectKind::Image => "image",
        holofs_model::manifest::ObjectKind::Audio => "audio",
        holofs_model::manifest::ObjectKind::Text => "text",
        holofs_model::manifest::ObjectKind::Opaque => "opaque",
        holofs_model::manifest::ObjectKind::Directory => "directory",
    };
    Ok(FileMetricsView {
        kind: kind_str.into(),
        total_shards_in_file: m.total_shards_in_file,
        unique_shards_in_file: m.unique_shards_in_file,
        file_dedup_savings_pct: m.file_dedup_savings_pct,
        catalog_total_shards: m.catalog_total_shards,
        catalog_unique_shards: m.catalog_unique_shards,
        unique_to_file: m.unique_to_file,
        originality_pct: m.originality_pct,
        originality_per_layer: m.originality_per_layer,
        neighbours: m
            .neighbours
            .into_iter()
            .map(|n| NeighbourRow {
                kind: match n.kind {
                    holofs_model::manifest::ObjectKind::Image => "image".into(),
                    holofs_model::manifest::ObjectKind::Audio => "audio".into(),
                    holofs_model::manifest::ObjectKind::Text => "text".into(),
                    holofs_model::manifest::ObjectKind::Opaque => "opaque".into(),
                    holofs_model::manifest::ObjectKind::Directory => "directory".into(),
                },
                name: n.name,
                shared_total: n.shared_total,
                shared_per_layer: n.shared_per_layer,
                overlap_pct: n.overlap_pct,
            })
            .collect(),
        layer_energy: m.layer_energy,
        audio_bands: m.audio_bands.map(|a| (a.bass, a.mid, a.treble)),
    })
}

// ===== Components ==========================================================

/// `GET /health` — cluster overview + per-node kill/revive table.
#[component]
pub fn HealthIndexPage() -> impl IntoView {
    let data = Resource::new(|| (), |()| async move { get_health_index().await });

    view! {
        <crate::ui::Topbar active="health"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("health.loading")}</p> }>
                {move || data.get().map(|res| match res {
                    Ok(idx) => view! { <HealthIndexBody data=idx/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn HealthIndexBody(data: HealthIndex) -> impl IntoView {
    let HealthIndex {
        nodes,
        objects,
        n_live,
        n_total,
    } = data;
    let initial = HealthSnapshot {
        n_live,
        n_total,
        objects: objects.len(),
        ts_ms: 0,
    };

    view! {
        <LiveClusterStats initial=initial/>

        <h2>{t!("health.cluster_nodes_h")}</h2>
        <table class="health-nodes">
            <thead>
                <tr>
                    <th>"#"</th>
                    <th class="name">{t!("health.col.address")}</th>
                    <th>{t!("health.col.zone")}</th>
                    <th>{t!("health.col.status")}</th>
                    <th>{t!("health.col.action")}</th>
                </tr>
            </thead>
            <tbody>
                <For
                    each=move || nodes.clone()
                    key=|n| n.idx
                    children=|n| view! { <NodeRowView node=n/> }
                />
            </tbody>
        </table>

        // the per-object list used to live here as a flat
        // `<ul>`, but the catalog page already lists every object with
        // a per-row `health` action — this list duplicated it with
        // less metadata and no UI actions. Keep a one-line summary
        // pointing back to the catalog instead.
        <p class="mut health-objects-line">
            {move || objects.len().to_string()} " " {t!("health.objects_h")}
            " · " <a href="/">{t!("health.see_catalog")} " →"</a>
        </p>
    }
}

/// Cluster-stat cards that update live from `GET /api/health/events`.
///
/// The initial render comes from the SSR'd `HealthIndex` snapshot so the
/// page is correct on first paint. After hydration runs in the browser, an
/// `EventSource` subscribes to the SSE stream and patches the signal every
/// ~3 seconds. Under SSR the effect is a no-op (Leptos does not fire
/// effects server-side), so we don't open a phantom subscription per render.
#[component]
fn LiveClusterStats(initial: HealthSnapshot) -> impl IntoView {
    let snap = RwSignal::new(initial);
    // Track SSE connection state so the "●" indicator (and its
    // screen-reader label) reflect reality — pre-review it always
    // pulsed green even after the browser dropped the socket, giving
    // false confidence that the numbers were fresh.
    // 0 = connecting, 1 = live, 2 = disconnected/retrying.
    let sse_state = RwSignal::new(0u8);

    // Effect::new only runs client-side after hydrate. The body itself is
    // cfg-gated so the SSR build does not see web-sys types it cannot link
    // against (web-sys symbols exist only on wasm32 targets).
    Effect::new(move |_| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::closure::Closure;
            use wasm_bindgen::JsCast;

            let Ok(source) = web_sys::EventSource::new("/api/health/events") else {
                sse_state.set(2);
                return;
            };
            let cb_msg = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                move |evt: web_sys::MessageEvent| {
                    // First message → we're live.
                    sse_state.set(1);
                    let Some(payload) = evt.data().as_string() else {
                        return;
                    };
                    if let Ok(parsed) = serde_json::from_str::<HealthSnapshot>(&payload) {
                        snap.set(parsed);
                    }
                },
            );
            let cb_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                sse_state.set(1);
            });
            // EventSource auto-reconnects with an increasing backoff on
            // network drops — we just want to reflect that in the UI.
            // Browser fires `error` on drop, keeps trying; state stays
            // "disconnected" until the next `open` / `message` lands.
            let cb_err = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                sse_state.set(2);
            });
            source.set_onopen(Some(cb_open.as_ref().unchecked_ref()));
            source.set_onmessage(Some(cb_msg.as_ref().unchecked_ref()));
            source.set_onerror(Some(cb_err.as_ref().unchecked_ref()));
            // Leak both the closure and the EventSource so they live for the
            // lifetime of the page. Closing the tab tears them down.
            cb_msg.forget();
            cb_open.forget();
            cb_err.forget();
            std::mem::forget(source);
        }
    });

    let live_label = move || {
        let s = snap.get();
        format!("{}/{}", s.n_live, s.n_total)
    };
    let disabled_label = move || {
        let s = snap.get();
        (s.n_total - s.n_live).to_string()
    };
    let objects_label = move || snap.get().objects.to_string();
    let live_class = move || {
        let s = snap.get();
        if s.n_live == s.n_total {
            "v ok"
        } else {
            "v partial"
        }
    };
    let sse_class = move || match sse_state.get() {
        1 => "v mono ok",
        2 => "v mono bad",
        _ => "v mono mut",
    };
    let sse_label = move || match sse_state.get() {
        1 => t!("health.stat.live_sse"),
        2 => t!("health.stat.sse_disconnected"),
        _ => t!("health.stat.sse_connecting"),
    };
    let sse_aria = move || match sse_state.get() {
        1 => "live",
        2 => "disconnected",
        _ => "connecting",
    };

    view! {
        // aria-live="polite" so screen readers announce updates
        // without stealing focus. Only these stat cards actually
        // change post-hydrate — the rest of the page is static.
        <section class="cluster-stats" aria-live="polite">
            <div class="stat">
                <div class=live_class>{live_label}</div>
                <div class="l">{t!("health.stat.live_total")}</div>
            </div>
            <div class="stat">
                <div class="v">{objects_label}</div>
                <div class="l">{t!("health.objects_h")}</div>
            </div>
            <div class="stat">
                <div class="v">{disabled_label}</div>
                <div class="l">{t!("health.stat.admin_disabled")}</div>
            </div>
            <div class="stat live-hint">
                <div class=sse_class role="img" aria-label=sse_aria>"●"</div>
                <div class="l">{sse_label}</div>
            </div>
        </section>
    }
}

#[component]
fn NodeRowView(node: NodeRow) -> impl IntoView {
    let NodeRow {
        idx,
        addr,
        zone,
        admin_killed,
    } = node;
    let (status_label, status_class) = if admin_killed {
        (t!("health.status.disabled"), "bad")
    } else {
        (t!("health.status.live"), "ok")
    };
    let button_label = if admin_killed {
        t!("health.action.revive")
    } else {
        t!("health.action.kill")
    };
    view! {
        <tr>
            <td>{idx}</td>
            <td class="name"><code>{addr}</code></td>
            <td>{zone}</td>
            <td class=status_class>{status_label}</td>
            <td>
                <form method="POST" action="/admin/node">
                    <input type="hidden" name="i" value=idx.to_string()/>
                    <button type="submit">{button_label}</button>
                </form>
            </td>
        </tr>
    }
}

/// `GET /health/:name` — per-object margin + Monte Carlo + zone failures.
#[component]
pub fn HealthDetailPage() -> impl IntoView {
    let params = use_params_map();
    let name = move || params.with(|p| p.get("name").unwrap_or_default());

    let data = Resource::new(name, |n| async move {
        if n.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing name".into(),
            ))
        } else {
            get_object_health(n).await
        }
    });

    view! {
        <crate::ui::Topbar active="health"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("health.loading_object")}</p> }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <HealthDetailBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn HealthDetailBody(data: ObjectHealthView) -> impl IntoView {
    let ObjectHealthView {
        name,
        object_id: _,
        cid_hex,
        k,
        channels,
        nlayers,
        n_live,
        n_total,
        current_resolution,
        layers,
        scenarios,
        zone_failures,
    } = data;
    let resolution_label = match current_resolution {
        Some(l) if l + 1 == nlayers => format!("full (L0-L{l})"),
        Some(l) => format!("L0-L{l}"),
        None => "DEAD".into(),
    };
    let resolution_class = if current_resolution.is_some() {
        "v ok"
    } else {
        "v bad"
    };
    let cid_short: String = cid_hex.chars().take(16).collect();

    let metrics_name = name.clone();
    let metrics = Resource::new(
        move || metrics_name.clone(),
        |n| async move { get_file_metrics(n).await },
    );

    view! {
        <p><a href="/health">"← /health"</a></p>
        <h2>{name.clone()}</h2>
        <section class="cluster-stats">
            <div class="stat"><div class="v">{n_live}"/"{n_total}</div><div class="l">{t!("health.stat.live_total")}</div></div>
            <div class="stat"><div class="v">{k}</div><div class="l">{t!("health.stat.k_threshold")}</div></div>
            <div class="stat"><div class="v">{channels}"×"{nlayers}</div><div class="l">{t!("health.stat.channels_layers")}</div></div>
            <div class="stat"><div class=resolution_class>{resolution_label}</div><div class="l">{t!("health.stat.current_res")}</div></div>
            <div class="stat"><div class="v mono">{cid_short}</div><div class="l">"data_cid"</div></div>
        </section>

        <h2>{t!("health.matrix_h")}</h2>
        // A9: plain-English intro + colour-swatch legend for the
        // ok/partial/bad cell classes. Was "wall of numbers" before —
        // an engineer parses it, /about promises "readable by
        // non-specialists". The legend uses the same tokens the
        // cells use (--ok-bg / --warn-bg / --bad-bg from PR-1).
        <p class="dashboard-intro">{t!("health.matrix_intro")}</p>
        <div class="dashboard-legend">
            <span><i class="swatch ok"></i>{t!("health.legend.ok")}</span>
            <span><i class="swatch partial"></i>{t!("health.legend.partial")}</span>
            <span><i class="swatch bad"></i>{t!("health.legend.bad")}</span>
        </div>
        <LayerMatrix layers=layers channels=channels nlayers=nlayers k=k/>

        {(!scenarios.is_empty()).then(|| view! {
            <h2>{t!("health.loss_h")}</h2>
            <p class="dashboard-intro">{t!("health.loss_intro")}</p>
            <LossTable scenarios=scenarios nlayers=nlayers/>
        })}

        {(!zone_failures.is_empty()).then(|| view! {
            <h2>{t!("health.zone_h")}</h2>
            <p class="dashboard-intro">{t!("health.zone_intro")}</p>
            <ZoneTable rows=zone_failures/>
        })}

        <h2>{t!("health.metrics_h")}</h2>
        <p class="mut">{t!("health.metrics_intro")}</p>
        <Suspense fallback=move || view! {
            <p class="mut">{t!("health.metrics_loading")}</p>
        }>
            {move || metrics.get().map(|res| match res {
                Ok(m) => view! { <FileMetricsBlock data=m/> }.into_any(),
                Err(e) => view! {
                    <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn FileMetricsBlock(data: FileMetricsView) -> impl IntoView {
    let FileMetricsView {
        kind,
        total_shards_in_file,
        unique_shards_in_file,
        file_dedup_savings_pct,
        catalog_total_shards,
        catalog_unique_shards,
        unique_to_file,
        originality_pct,
        originality_per_layer,
        neighbours,
        layer_energy,
        audio_bands,
    } = data;

    let catalog_savings_pct = if catalog_total_shards > 0 {
        (catalog_total_shards - catalog_unique_shards) as f32 * 100.0
            / catalog_total_shards as f32
    } else {
        0.0
    };
    let originality_label = format!("{originality_pct:.1}%");
    let originality_class = if originality_pct >= 50.0 {
        "v ok"
    } else if originality_pct >= 15.0 {
        "v partial"
    } else {
        "v bad"
    };
    let neighbours_for_table = neighbours.clone();
    let layer_energy_for_chart = layer_energy.clone();
    let layer_energy_for_audio_check = layer_energy.clone();

    view! {
        // --- Card row: storage / dedup / originality.
        <section class="cluster-stats">
            <div class="stat">
                <div class="v">{unique_shards_in_file.to_string()} "/" {total_shards_in_file.to_string()}</div>
                <div class="l">{t!("health.metrics.shards_in_file")}</div>
            </div>
            <div class="stat">
                <div class="v">{format!("{file_dedup_savings_pct:.1}%")}</div>
                <div class="l">{t!("health.metrics.file_dedup")}</div>
            </div>
            <div class="stat">
                <div class=originality_class>{originality_label}</div>
                <div class="l">{t!("health.metrics.originality")}</div>
            </div>
            <div class="stat">
                <div class="v">{unique_to_file.to_string()}</div>
                <div class="l">{t!("health.metrics.unique_to_file")}</div>
            </div>
            <div class="stat">
                <div class="v">{catalog_unique_shards.to_string()} "/" {catalog_total_shards.to_string()}</div>
                <div class="l">{t!("health.metrics.catalog_dedup_raw")}</div>
            </div>
            <div class="stat">
                <div class="v">{format!("{catalog_savings_pct:.1}%")}</div>
                <div class="l">{t!("health.metrics.catalog_savings")}</div>
            </div>
        </section>

        // --- Originality per layer (bar chart).
        {(!originality_per_layer.is_empty()).then(|| view! {
            <h3 class="metrics-h">{t!("health.metrics.layer_originality_h")}</h3>
            <p class="mut metrics-sub">{t!("health.metrics.layer_originality_help")}</p>
            <LayerBars values=originality_per_layer.clone() unit="%".to_string() max_hint=Some(100.0)/>
        })}

        // --- Decoded layer energy bars.
        {layer_energy_for_chart.as_ref().map(|e| {
            let e = e.clone();
            view! {
                <h3 class="metrics-h">{t!("health.metrics.layer_energy_h")}</h3>
                <p class="mut metrics-sub">{t!("health.metrics.layer_energy_help")}</p>
                <EnergyBars values=e/>
            }
        })}

        // --- Audio bands (bass / mid / treble) — derived from layer_energy.
        {(kind == "audio").then(|| audio_bands.map(|(bass, mid, treble)| view! {
            <h3 class="metrics-h">{t!("health.metrics.audio_bands_h")}</h3>
            <p class="mut metrics-sub">{t!("health.metrics.audio_bands_help")}</p>
            <AudioBands bass=bass mid=mid treble=treble/>
        })).flatten()}

        // --- Neighbours table.
        {(!neighbours_for_table.is_empty()).then(|| view! {
            <h3 class="metrics-h">{t!("health.metrics.neighbours_h")}</h3>
            <p class="mut metrics-sub">{t!("health.metrics.neighbours_help")}</p>
            <NeighboursTable rows=neighbours_for_table.clone()/>
        })}

        {(neighbours.is_empty() && layer_energy_for_audio_check.is_none()).then(|| view! {
            <p class="mut">{t!("health.metrics.empty")}</p>
        })}
    }
}

#[component]
fn LayerBars(values: Vec<f32>, unit: String, max_hint: Option<f32>) -> impl IntoView {
    let max_val: f32 = max_hint.unwrap_or_else(|| {
        values
            .iter()
            .cloned()
            .fold(0.0_f32, |a, b| a.max(b))
            .max(1.0)
    });
    let max_val = max_val.max(1e-6);
    view! {
        <div class="layer-bars">
            {values.into_iter().enumerate().map(|(i, v)| {
                let pct = (v / max_val * 100.0).clamp(0.0, 100.0);
                let label = format!("L{i}");
                let value_label = format!("{v:.1}{}", unit);
                view! {
                    <div class="layer-bar">
                        <div class="lb-label">{label}</div>
                        <div class="lb-track">
                            <div class="lb-fill" style=format!("width:{pct:.1}%")></div>
                        </div>
                        <div class="lb-value mono">{value_label}</div>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
fn EnergyBars(values: Vec<f64>) -> impl IntoView {
    let total: f64 = values.iter().sum::<f64>().max(1e-12);
    view! {
        <div class="layer-bars">
            {values.into_iter().enumerate().map(|(i, v)| {
                let pct = (v / total * 100.0).clamp(0.0, 100.0);
                let label = format!("L{i}");
                let value_label = format!("{pct:.1}%");
                view! {
                    <div class="layer-bar">
                        <div class="lb-label">{label}</div>
                        <div class="lb-track">
                            <div class="lb-fill" style=format!("width:{pct:.1}%")></div>
                        </div>
                        <div class="lb-value mono">{value_label}</div>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
fn AudioBands(bass: f64, mid: f64, treble: f64) -> impl IntoView {
    let total = (bass + mid + treble).max(1e-12);
    let row = |label: &str, v: f64| {
        let pct = (v / total * 100.0).clamp(0.0, 100.0);
        let val = format!("{pct:.1}%");
        view! {
            <div class="layer-bar">
                <div class="lb-label">{label.to_string()}</div>
                <div class="lb-track">
                    <div class="lb-fill" style=format!("width:{pct:.1}%")></div>
                </div>
                <div class="lb-value mono">{val}</div>
            </div>
        }
    };
    view! {
        <div class="layer-bars">
            {row("bass", bass)}
            {row("mid", mid)}
            {row("treble", treble)}
        </div>
    }
}

#[component]
fn NeighboursTable(rows: Vec<NeighbourRow>) -> impl IntoView {
    let max_shared: u64 = rows.iter().map(|r| r.shared_total).max().unwrap_or(1).max(1);
    view! {
        <table class="metrics-neighbours">
            <thead>
                <tr>
                    <th class="name">{t!("health.metrics.col.name")}</th>
                    <th>{t!("health.metrics.col.kind")}</th>
                    <th>{t!("health.metrics.col.shared")}</th>
                    <th>{t!("health.metrics.col.overlap_pct")}</th>
                    <th>{t!("health.metrics.col.layer_breakdown")}</th>
                </tr>
            </thead>
            <tbody>
                {rows.into_iter().map(|r| {
                    let layer_total = r.shared_per_layer.iter().copied().map(u64::from).sum::<u64>().max(1);
                    let segments: Vec<_> = r.shared_per_layer.iter().enumerate().map(|(i, &n)| {
                        let pct = n as f32 * 100.0 / layer_total as f32;
                        let title = format!("L{i}: {n} shards");
                        view! {
                            <span
                                class="layer-seg"
                                style=format!("flex:{pct:.2}")
                                title=title
                            >
                                {(pct >= 8.0).then(|| format!("L{i}"))}
                            </span>
                        }
                    }).collect();
                    let strength_pct = r.shared_total as f32 * 100.0 / max_shared as f32;
                    let encoded_name = crate::url_encode(&r.name);
                    view! {
                        <tr>
                            <td class="name">
                                <a href={format!("/{encoded_name}")} rel="external">
                                    {r.name.clone()}
                                </a>
                            </td>
                            <td class="mut"><code>{r.kind.clone()}</code></td>
                            <td>
                                <div class="lb-track" style="width:80px;display:inline-block">
                                    <div class="lb-fill" style=format!("width:{strength_pct:.1}%")></div>
                                </div>
                                " "
                                <code>{r.shared_total.to_string()}</code>
                            </td>
                            <td><code>{format!("{:.1}%", r.overlap_pct)}</code></td>
                            <td>
                                <div class="layer-breakdown">{segments}</div>
                            </td>
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
    }
}

#[component]
fn LayerMatrix(layers: Vec<LayerRow>, channels: u8, nlayers: u8, k: u16) -> impl IntoView {
    let lookup = move |c: u8, l: u8| -> Option<LayerRow> {
        layers
            .iter()
            .find(|row| row.channel == c && row.layer == l)
            .cloned()
    };

    view! {
        <table class="health-layers">
            <thead>
                <tr>
                    // `layer` column carries text labels (`L0 · LL (coarse)`)
                    // and its body cells use `class="name"` for the
                    // left-aligned look — pair the header up so both
                    // align identically.
                    <th class="name">{t!("health.col.layer")}</th>
                    {(0..channels).map(|c| view! { <th>{t!("health.col.channel_short")} " " {c}</th> }).collect_view()}
                </tr>
            </thead>
            <tbody>
                {(0..nlayers).map(|l| {
                    let label = match l {
                        0 => format!("L{l} · LL (coarse)"),
                        x if x + 1 == nlayers => format!("L{l} · HH (fine)"),
                        _ => format!("L{l}"),
                    };
                    let lookup = lookup.clone();
                    view! {
                        <tr>
                            <td class="name">{label}</td>
                            {(0..channels).map(|c| {
                                match lookup(c, l) {
                                    Some(row) => {
                                        let cls = if row.margin < 0 { "bad" }
                                                  else if row.margin == 0 { "partial" }
                                                  else { "ok" };
                                        let cell = format!("{}/{}  ({:+})", row.n_alive, k, row.margin);
                                        view! { <td class=cls><code>{cell}</code></td> }.into_any()
                                    }
                                    None => view! { <td class="mut">"-"</td> }.into_any(),
                                }
                            }).collect_view()}
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
    }
}

#[component]
fn LossTable(scenarios: Vec<LossRow>, nlayers: u8) -> impl IntoView {
    // Header: dead | L0 | L0-L1 | … | all
    let mut col_headers: Vec<String> = Vec::with_capacity(nlayers as usize + 1);
    col_headers.push("dead".into());
    for l in 0..nlayers {
        col_headers.push(if l + 1 == nlayers {
            format!("L0-L{l} (full)")
        } else {
            format!("L0-L{l}")
        });
    }

    view! {
        <table class="health-loss">
            <thead>
                <tr>
                    <th>{t!("health.col.kill_pct")}</th>
                    <th>{t!("health.col.trials")}</th>
                    {col_headers.into_iter().map(|h| view! { <th>{h}</th> }).collect_view()}
                    // A9: extra "distribution" column that draws the row's
                    // full pmf as a single horizontal stacked bar — red
                    // = dead (index 0), orange = preview-only (1), green
                    // = every fuller resolution. Lets a non-specialist
                    // read the shape at a glance without parsing five
                    // percent-columns.
                    <th>{t!("health.col.pmf_bar")}</th>
                </tr>
            </thead>
            <tbody>
                {scenarios.into_iter().map(|s| {
                    let pmf = s.pmf.clone();
                    let pmf_for_bar = s.pmf.clone();
                    view! {
                        <tr>
                            <td><code>{s.kill_pct}"%"</code></td>
                            <td class="mut"><code>{s.n_trials}</code></td>
                            {pmf.into_iter().enumerate().map(|(i, p)| {
                                let pct = p * 100.0;
                                let cls = if i == 0 && pct > 0.0 { "bad" }
                                          else if pct > 0.0 { "" }
                                          else { "mut" };
                                let body = if pct >= 0.1 {
                                    format!("{pct:.1}%")
                                } else if pct > 0.0 {
                                    "<0.1%".to_string()
                                } else {
                                    "-".to_string()
                                };
                                view! { <td class=cls><code>{body}</code></td> }
                            }).collect_view()}
                            <td>
                                <div class="pmf-bar" role="img"
                                     aria-label={pmf_bar_aria(&pmf_for_bar)}>
                                    {pmf_for_bar.clone().into_iter().enumerate().map(|(i, p)| {
                                        let pct = p * 100.0;
                                        // Skip zero segments so the bar
                                        // doesn't render invisible 0-width
                                        // dividers (they'd still count as
                                        // border-only slivers).
                                        if pct < 0.05 { return None; }
                                        let cls = if i == 0 { "dead" }
                                                  else if i == 1 { "partial" }
                                                  else { "ok" };
                                        let title = format!("L{}: {pct:.1}%",
                                            if i == 0 { "0 (lost)".to_string() } else { (i - 1).to_string() });
                                        Some(view! {
                                            <span class={format!("pmf-seg {cls}")}
                                                  style=format!("flex-basis:{pct:.2}%")
                                                  title=title></span>
                                        })
                                    }).collect_view()}
                                </div>
                            </td>
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
    }
}

fn pmf_bar_aria(pmf: &[f32]) -> String {
    // Screen-reader label for the stacked bar. Reads out each
    // resolution + its probability so blind users get the same info
    // as the visual glance.
    let mut s = String::from("distribution: ");
    for (i, p) in pmf.iter().enumerate() {
        let pct = p * 100.0;
        if pct < 0.05 {
            continue;
        }
        let name = if i == 0 { "dead".to_string() } else { format!("L0-L{}", i - 1) };
        s.push_str(&format!("{name} {pct:.0}%, "));
    }
    s.trim_end_matches(", ").to_string()
}

#[component]
fn ZoneTable(rows: Vec<ZoneFailureRow>) -> impl IntoView {
    view! {
        <table class="health-zones">
            <thead>
                <tr>
                    <th>{t!("health.col.zone")}</th>
                    <th>{t!("health.col.nodes_in_zone")}</th>
                    <th>{t!("health.col.remaining_res")}</th>
                </tr>
            </thead>
            <tbody>
                {rows.into_iter().map(|z| {
                    let (label, cls) = match z.resolution {
                        Some(l) => (format!("L0-L{l}"), "ok"),
                        None => ("DEAD".to_string(), "bad"),
                    };
                    view! {
                        <tr>
                            <td>{z.zone}</td>
                            <td>{z.nodes_in_zone}</td>
                            <td class=cls>{label}</td>
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
    }
}
