//! `/inspect/:name` and `/inspect/:name/zoom/:c_l_idx` — Leptos pages showing
//! the physical shard layout of one object.
//!
//! Both pages call server functions that wrap `Gateway::inspect` and
//! `Gateway::shard_payload`. The companion `/inspect/:name/shard/:c_l_idx.png`
//! handler is a plain axum binary endpoint (see `handlers::get_shard_png`).

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::t;

// ===== View-models (Send across SSR ⇄ hydrate boundary) ====================

/// One shard placement on a node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardView {
    pub idx: u32,
    pub node_idx: usize,
    pub node_addr: String,
    pub is_systematic: bool,
}

/// All shards for one (channel, layer) pair.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayerView {
    pub channel: u8,
    pub layer: u8,
    pub n_shards: u32,
    pub k_systematic: u16,
    pub shards: Vec<ShardView>,
}

/// Backing data for `/inspect/:name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InspectView {
    pub name: String,
    pub kind: String,
    pub channels: u8,
    pub nlayers: u8,
    pub k: u16,
    pub layers: Vec<LayerView>,
}

/// Backing data for `/inspect/:name/zoom/:c_l_idx`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZoomView {
    pub name: String,
    pub channel: u8,
    pub layer: u8,
    pub idx: u32,
    pub is_systematic: bool,
    pub node_idx: usize,
    pub node_addr: String,
    pub sym_len: u32,
    pub hash_hex: String,
    /// `Some` when the shard was retrieved; `None` if the node is dead/lost.
    pub shard: Option<ZoomShard>,
}

/// Shard payload + coeffs included on a successful zoom fetch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZoomShard {
    pub coeffs_hex: String,
    pub payload_len: usize,
    pub payload_preview_hex: String,
}

// ===== Server functions ====================================================

/// Inspect view-model: layout of every shard for one object.
#[server(
    name = GetInspect,
    prefix = "/api",
    endpoint = "inspect",
)]
pub async fn get_inspect(name: String) -> Result<InspectView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let info = gw
        .inspect(&name)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(inspect_to_view(info))
}

/// Single-shard zoom view-model: shard metadata + hex coeffs + payload preview.
#[server(
    name = GetInspectZoom,
    prefix = "/api",
    endpoint = "inspect_zoom",
)]
pub async fn get_inspect_zoom(
    name: String,
    c: u8,
    l: u8,
    idx: u32,
) -> Result<ZoomView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let payload = gw
        .shard_payload(&name, c, l, idx)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let info = gw
        .inspect(&name)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    // Recover node + sym_len even when the shard itself is missing.
    let (node_idx, node_addr, sym_len, is_systematic, hash_hex) = if let Some(p) = &payload {
        (
            p.node_idx,
            p.node_addr.clone(),
            p.sym_len,
            p.is_systematic,
            p.hash_hex.clone(),
        )
    } else {
        let layer = info
            .layers
            .iter()
            .find(|ly| ly.channel == c && ly.layer == l);
        let shard = layer.and_then(|ly| ly.shards.iter().find(|s| s.idx == idx));
        let node_idx = shard.map(|s| s.node_idx).unwrap_or(0);
        let node_addr = shard.map(|s| s.node_addr.clone()).unwrap_or_default();
        let is_systematic = shard.map(|s| s.is_systematic).unwrap_or(idx < u32::from(info.k));
        (node_idx, node_addr, 0u32, is_systematic, String::new())
    };
    let shard_view = payload.map(|p| ZoomShard {
        coeffs_hex: hex_block(&p.coeffs, 16),
        payload_len: p.payload.len(),
        payload_preview_hex: hex_block(&p.payload[..p.payload.len().min(64)], 16),
    });
    Ok(ZoomView {
        name,
        channel: c,
        layer: l,
        idx,
        is_systematic,
        node_idx,
        node_addr,
        sym_len,
        hash_hex,
        shard: shard_view,
    })
}

// ===== SSR-only helpers ====================================================

#[cfg(feature = "ssr")]
fn inspect_to_view(info: holofs_gateway::InspectInfo) -> InspectView {
    use holofs_model::manifest::ObjectKind;
    let kind = match info.kind {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
    .to_string();
    let layers = info
        .layers
        .into_iter()
        .map(|ly| LayerView {
            channel: ly.channel,
            layer: ly.layer,
            n_shards: ly.n_shards,
            k_systematic: ly.k_systematic,
            shards: ly
                .shards
                .into_iter()
                .map(|s| ShardView {
                    idx: s.idx,
                    node_idx: s.node_idx,
                    node_addr: s.node_addr,
                    is_systematic: s.is_systematic,
                })
                .collect(),
        })
        .collect();
    InspectView {
        name: info.name,
        kind,
        channels: info.channels,
        nlayers: info.nlayers,
        k: info.k,
        layers,
    }
}

/// Format bytes as `XX ` with a newline every `per_line` — same shape as the
/// legacy gateway's `hex_block`.
#[cfg(feature = "ssr")]
fn hex_block(bytes: &[u8], per_line: usize) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % per_line == 0 {
            out.push('\n');
        }
        out.push_str(&format!("{b:02x} "));
    }
    out
}

// ===== Components ==========================================================

/// `GET /inspect/:name` — grid of every shard, grouped by channel and layer.
#[component]
pub fn InspectPage() -> impl IntoView {
    let params = use_params_map();
    let name = move || params.with(|p| p.get("name").unwrap_or_default());

    let data = Resource::new(name, |n| async move {
        if n.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing name".into(),
            ))
        } else {
            get_inspect(n).await
        }
    });

    view! {
        <crate::ui::Topbar active="catalog"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("inspect.loading")}</p> }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <InspectBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn InspectBody(data: InspectView) -> impl IntoView {
    let InspectView {
        name,
        kind,
        channels,
        nlayers,
        k: _,
        layers,
    } = data;
    let enc = crate::url_encode(&name);

    // Pre-group layers by channel for sequential rendering.
    let mut by_channel: Vec<Vec<LayerView>> = (0..channels).map(|_| Vec::new()).collect();
    for ly in layers {
        if (ly.channel as usize) < by_channel.len() {
            by_channel[ly.channel as usize].push(ly);
        }
    }
    let kind_for_labels = kind.clone();
    let enc_for_links = enc.clone();
    let enc_for_footer = enc.clone();

    view! {
        <h2 style="margin-top:0">{name.clone()} " " {t!("inspect.title_suffix")}</h2>
        <p class="mut">{t!("inspect.intro")}</p>

        {by_channel.into_iter().enumerate().map(|(c, layers)| {
            let ch_label = channel_label(&kind_for_labels, c as u8);
            let kind_for_layers = kind_for_labels.clone();
            let enc = enc_for_links.clone();
            view! {
                <h3 class="inspect-ch">{ch_label}</h3>
                {layers.into_iter().map(move |ly| {
                    let LayerView { channel, layer, n_shards, k_systematic, shards } = ly;
                    let layer_label = layer_label(&kind_for_layers, layer, nlayers);
                    let kk = u32::from(k_systematic);
                    let n_rlnc = n_shards.saturating_sub(kk);
                    let enc = enc.clone();
                    view! {
                        <h4 class="inspect-layer">
                            {t!("inspect.layer_prefix")} " " {layer} " — " {layer_label} " · " {n_shards} " " {t!("inspect.shards_word")} " ("
                            {kk} " " {t!("inspect.systematic_word")} " + " {n_rlnc} " " {t!("inspect.rlnc_word")} ")"
                        </h4>
                        <div class="shard-grid">
                            {shards.into_iter().map(|s| {
                                let cls = if s.is_systematic { "shard-cell sys" } else { "shard-cell rlnc" };
                                let title = format!("shard #{} → node {} (click to zoom)", s.idx, s.node_idx);
                                let href = format!("/inspect-zoom/{channel}_{layer}_{}/{enc}", s.idx);
                                let img = format!("/api/shard/{channel}_{layer}_{}.png/{enc}", s.idx);
                                let label = format!("n{}", s.node_idx);
                                view! {
                                    <a class=cls title=title href=href>
                                        <img loading="lazy" src=img alt=""/>
                                        <span class="idx">{label}</span>
                                    </a>
                                }
                            }).collect_view()}
                        </div>
                    }
                }).collect_view()}
            }
        }).collect_view()}

        <p>
            <a href={format!("/health/{enc_for_footer}")}>"← " {t!("nav.health")}</a> " · "
            <a href="/">{t!("generic.back_to_catalog")}</a>
        </p>
    }
}

/// `GET /inspect/:name/zoom/:c_l_idx` — enlarged shard + hex coeffs/payload.
#[component]
pub fn InspectZoomPage() -> impl IntoView {
    let params = use_params_map();
    let resource = Resource::new(
        move || {
            params.with(|p| {
                (
                    p.get("name").unwrap_or_default(),
                    p.get("c_l_idx").unwrap_or_default(),
                )
            })
        },
        |(name, c_l_idx)| async move {
            if name.is_empty() || c_l_idx.is_empty() {
                return Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                    "missing path params".into(),
                ));
            }
            let trimmed = c_l_idx.trim_end_matches(".png");
            let parts: Vec<&str> = trimmed.split('_').collect();
            if parts.len() != 3 {
                return Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                    format!("bad c_l_idx: {c_l_idx}"),
                ));
            }
            let c = parts[0].parse::<u8>().map_err(|e| {
                ServerFnError::<server_fn::error::NoCustomError>::ServerError(format!("bad channel: {e}"))
            })?;
            let l = parts[1].parse::<u8>().map_err(|e| {
                ServerFnError::<server_fn::error::NoCustomError>::ServerError(format!("bad layer: {e}"))
            })?;
            let idx = parts[2].parse::<u32>().map_err(|e| {
                ServerFnError::<server_fn::error::NoCustomError>::ServerError(format!("bad idx: {e}"))
            })?;
            get_inspect_zoom(name, c, l, idx).await
        },
    );

    view! {
        <crate::ui::Topbar active="catalog"/>

        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("inspect.loading_shard")}</p> }>
                {move || resource.get().map(|res| match res {
                    Ok(v) => view! { <ZoomBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn ZoomBody(data: ZoomView) -> impl IntoView {
    let ZoomView {
        name,
        channel,
        layer,
        idx,
        is_systematic,
        node_idx,
        node_addr,
        sym_len,
        hash_hex,
        shard,
    } = data;
    let enc = crate::url_encode(&name);
    let kind_class = if is_systematic { "sys" } else { "rlnc" };
    let kind_label = if is_systematic { "SYSTEMATIC" } else { "RLNC" };
    // the long descriptive blurb (sys vs RLNC explanation)
    // stays in English for now. Translating "coeffs = e_i = one in the
    // i-th position" meaningfully needs domain-specific phrasing that
    // we can pass through later as a polish step.
    let kind_descr = if is_systematic {
        "this is the i-th raw data chunk (coeffs = e_i = one in the i-th position, zero elsewhere). Given any K systematic shards of a layer, recovery is concatenation."
    } else {
        "random linear combination over GF(256): coeffs = K random bytes, payload = Σ coeffs[i]·chunk[i]. Knowing only this shard you cannot recover anything."
    };
    let img_class = format!("zoom-img {kind_class}");
    let tag_class = format!("zoom-tag {kind_class}");
    let img_src = format!("/api/shard/{channel}_{layer}_{idx}.png/{enc}");
    let back_href = format!("/inspect/{enc}");

    view! {
        <h2 style="margin-top:0">
            {t!("inspect.shard_of")} " "
            <a href={back_href.clone()}>{name.clone()}</a>
        </h2>
        <div class="zoom-wrap">
            <div class=img_class>
                <img src=img_src alt=""/>
            </div>
            <div class="zoom-meta">
                <p><span class=tag_class>{kind_label}</span></p>
                <p class="mut">{kind_descr}</p>
                <dl>
                    <dt>{t!("inspect.zoom.shard_idx")}</dt><dd>{idx}</dd>
                    <dt>{t!("inspect.zoom.channel")}</dt><dd>{channel}</dd>
                    <dt>{t!("inspect.zoom.layer")}</dt><dd>"L"{layer}</dd>
                    <dt>{t!("inspect.zoom.physical_node")}</dt>
                    <dd>"n"{node_idx} " · " <code>{node_addr}</code></dd>
                    <dt>{t!("inspect.zoom.sym_len")}</dt><dd>{sym_len} " " {t!("inspect.zoom.bytes")}</dd>
                    <dt>{t!("inspect.zoom.shard_hash")}</dt><dd><code>{hash_hex}</code></dd>
                </dl>
                {match shard {
                    Some(s) => view! {
                        <p><b>{t!("inspect.zoom.coeffs")}</b> " " {t!("inspect.zoom.coeffs_k_bytes")}</p>
                        <pre>{s.coeffs_hex}</pre>
                        <p>
                            <b>{t!("inspect.zoom.payload_prefix")}</b>
                            " " {t!("inspect.zoom.payload_first_64")}
                            " " {s.payload_len} " " {t!("inspect.zoom.as_hex")}
                        </p>
                        <pre>{s.payload_preview_hex}</pre>
                    }.into_any(),
                    None => view! {
                        <p class="bad">{t!("inspect.zoom.shard_unavailable")}</p>
                    }.into_any(),
                }}
            </div>
        </div>
        <p style="margin-top:24px">
            <a href={back_href}>{t!("inspect.zoom.back_link")}</a>
        </p>
    }
}

// ===== Label helpers (no SSR-only deps) ====================================

fn channel_label(kind: &str, c: u8) -> String {
    match (kind, c) {
        ("image", 0) => t!("inspect.label.image_r").to_string(),
        ("image", 1) => t!("inspect.label.image_g").to_string(),
        ("image", 2) => t!("inspect.label.image_b").to_string(),
        ("audio", 0) => t!("inspect.label.audio_l").to_string(),
        ("audio", 1) => t!("inspect.label.audio_r").to_string(),
        _ => format!("{} {c}", t!("inspect.zoom.channel")),
    }
}

fn layer_label(kind: &str, l: u8, nlayers: u8) -> String {
    let last = nlayers.saturating_sub(1);
    match (kind, l) {
        ("image", 0) => t!("inspect.label.image_coarse").to_string(),
        ("image", x) if x == last => format!("L{x} · {}", t!("inspect.label.image_fine")),
        ("image", _) => t!("inspect.label.audio_mid").to_string(),
        ("audio", 0) => t!("inspect.label.audio_bass").to_string(),
        ("audio", x) if x == last => t!("inspect.label.audio_high").to_string(),
        ("audio", _) => t!("inspect.label.audio_mid").to_string(),
        _ => String::new(),
    }
}
