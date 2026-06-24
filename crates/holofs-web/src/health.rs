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

        // Stage 11.23: the per-object list used to live here as a flat
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

    // Effect::new only runs client-side after hydrate. The body itself is
    // cfg-gated so the SSR build does not see web-sys types it cannot link
    // against (web-sys symbols exist only on wasm32 targets).
    Effect::new(move |_| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::closure::Closure;
            use wasm_bindgen::JsCast;

            let Ok(source) = web_sys::EventSource::new("/api/health/events") else {
                return;
            };
            let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                move |evt: web_sys::MessageEvent| {
                    let Some(payload) = evt.data().as_string() else {
                        return;
                    };
                    if let Ok(parsed) = serde_json::from_str::<HealthSnapshot>(&payload) {
                        snap.set(parsed);
                    }
                },
            );
            source.set_onmessage(Some(cb.as_ref().unchecked_ref()));
            // Leak both the closure and the EventSource so they live for the
            // lifetime of the page. Closing the tab tears them down.
            cb.forget();
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

    view! {
        <section class="cluster-stats">
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
                <div class="v mono">"●"</div>
                <div class="l">{t!("health.stat.live_sse")}</div>
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
        <LayerMatrix layers=layers channels=channels nlayers=nlayers k=k/>

        {(!scenarios.is_empty()).then(|| view! {
            <h2>{t!("health.loss_h")}</h2>
            <LossTable scenarios=scenarios nlayers=nlayers/>
        })}

        {(!zone_failures.is_empty()).then(|| view! {
            <h2>{t!("health.zone_h")}</h2>
            <ZoneTable rows=zone_failures/>
        })}
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
                </tr>
            </thead>
            <tbody>
                {scenarios.into_iter().map(|s| {
                    let pmf = s.pmf.clone();
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
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
    }
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
