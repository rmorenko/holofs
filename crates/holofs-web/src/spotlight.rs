//! `/spotlight?a=NAME&x=N&y=N&w=N&h=N` — holographic spotlight.
//!
//! ROI presets (corner quadrants, center, full-width strips) drive the
//! visible result via plain GET form submits — no JS required. The page
//! renders the composited PNG from `/api/spotlight.png` and shows a
//! schematic frame overlay so the user can see WHERE the spotlight
//! landed without squinting at coordinates.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::t;

/// `GET /spotlight`.
#[component]
pub fn SpotlightPage() -> impl IntoView {
    let query = use_query_map();
    let params = move || {
        query.with(|q| {
            (
                q.get("a").unwrap_or_default(),
                q.get("x")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.35),
                q.get("y")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.35),
                q.get("w")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.3),
                q.get("h")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.3),
                // Stage 14.1: ?mode=coeff selects the Haar-mask path,
                // anything else (or absent) → spatial composite.
                q.get("mode").unwrap_or_else(|| "spatial".to_string()),
            )
        })
    };

    view! {
        <crate::ui::Topbar active="catalog"/>
        <main class="container spotlight-page">
            {move || {
                let (a, x, y, w, h, mode) = params();
                if a.is_empty() {
                    view! {
                        <p class="bad">{t!("spotlight.missing_a")}</p>
                        <p><a href="/" rel="external">{t!("generic.back_to_catalog")} " →"</a></p>
                    }.into_any()
                } else {
                    view! { <SpotlightBody a=a x=x y=y w=w h=h mode=mode/> }.into_any()
                }
            }}
        </main>
    }
}

#[component]
fn SpotlightBody(a: String, x: f32, y: f32, w: f32, h: f32, mode: String) -> impl IntoView {
    let enc = crate::url_encode(&a);
    let api_src = format!(
        "/api/spotlight.png?name={enc}&x={x}&y={y}&w={w}&h={h}&mode={mode}"
    );
    let coarse_src = format!("/preview/{enc}");
    let full_href = format!("/{enc}");
    let a_for_form = a.clone();
    let a_for_title = a.clone();
    // ROI visualised as percentages so it lines up over the rendered
    // image regardless of its actual pixel resolution.
    let roi_left = (x * 100.0).clamp(0.0, 100.0);
    let roi_top = (y * 100.0).clamp(0.0, 100.0);
    let roi_width = (w * 100.0).clamp(0.0, 100.0);
    let roi_height = (h * 100.0).clamp(0.0, 100.0);

    view! {
        <p class="mut">
            <a href="/" rel="external">"← " {t!("generic.back_to_catalog")}</a>
        </p>
        <h2 class="spotlight-h">
            {t!("spotlight.title_prefix")} " "
            <a href=full_href rel="external">{a_for_title}</a>
        </h2>
        <p class="mut">{t!("spotlight.intro")}</p>

        <ModeToggle a=a_for_form.clone() x=x y=y w=w h=h active_mode=mode.clone()/>
        <PresetButtons a=a_for_form.clone() mode=mode.clone()/>

        <details class="spotlight-custom">
            <summary>{t!("spotlight.custom_summary")}</summary>
            <form class="spotlight-form" method="GET" action="/spotlight">
                <input type="hidden" name="a" value=a_for_form.clone()/>
                <input type="hidden" name="mode" value=mode.clone()/>
                <NumField label_key="spotlight.field.x" name="x" value=x/>
                <NumField label_key="spotlight.field.y" name="y" value=y/>
                <NumField label_key="spotlight.field.w" name="w" value=w/>
                <NumField label_key="spotlight.field.h" name="h" value=h/>
                <button type="submit" class="tree-control-btn">
                    {t!("spotlight.apply")}
                </button>
            </form>
        </details>

        <div class="spotlight-stage">
            <img class="spotlight-image" src=api_src alt=a.clone()/>
            <div
                class="spotlight-frame"
                style=format!(
                    "left:{roi_left:.2}%;top:{roi_top:.2}%;width:{roi_width:.2}%;height:{roi_height:.2}%"
                )
            ></div>
            <img
                class="spotlight-image-fallback"
                src=coarse_src
                alt=""
            />
        </div>
        <p class="mut spotlight-caption">
            {t!("spotlight.caption_prefix")} " "
            <code>{format!("({:.2}, {:.2}, {:.2}, {:.2})", x, y, w, h)}</code>
        </p>
    }
}

#[component]
fn PresetButtons(a: String, mode: String) -> impl IntoView {
    let a_owned = a;
    let mode_owned = mode;
    let preset = move |x: f32, y: f32, w: f32, h: f32, key: &'static str| {
        let href = format!(
            "/spotlight?a={}&x={x}&y={y}&w={w}&h={h}&mode={}",
            crate::url_encode(&a_owned),
            mode_owned,
        );
        view! {
            <a class="spotlight-preset" href=href rel="external">
                {crate::i18n::translate(key, &crate::i18n::current_locale())}
            </a>
        }
    };
    view! {
        <div class="spotlight-presets">
            {preset(0.35, 0.35, 0.30, 0.30, "spotlight.preset.center")}
            {preset(0.00, 0.00, 0.50, 0.50, "spotlight.preset.tl")}
            {preset(0.50, 0.00, 0.50, 0.50, "spotlight.preset.tr")}
            {preset(0.00, 0.50, 0.50, 0.50, "spotlight.preset.bl")}
            {preset(0.50, 0.50, 0.50, 0.50, "spotlight.preset.br")}
            {preset(0.00, 0.40, 1.00, 0.20, "spotlight.preset.hbar")}
            {preset(0.40, 0.00, 0.20, 1.00, "spotlight.preset.vbar")}
        </div>
    }
}

/// Stage 14.1: two-mode pill picker — spatial composite (default,
/// Stage 13.2) vs coefficient mask (Stage 14.1, exposes the Haar
/// reverse map).
#[component]
fn ModeToggle(
    a: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    active_mode: String,
) -> impl IntoView {
    let enc_a = crate::url_encode(&a);
    let build_href = move |m: &str| {
        format!("/spotlight?a={enc_a}&x={x}&y={y}&w={w}&h={h}&mode={m}")
    };
    let pill = |slug: &'static str, key: &'static str, active: bool, href: String| {
        let label = crate::i18n::translate(key, &crate::i18n::current_locale());
        if active {
            view! { <span class="scope-pill scope-active">{label}</span> }.into_any()
        } else {
            view! { <a class="scope-pill" href=href rel="external">{label}</a> }.into_any()
        }
    };
    let is_coeff = active_mode == "coeff";
    let spatial_href = build_href("spatial");
    let coeff_href = build_href("coeff");
    view! {
        <p class="scope-picker mut spotlight-modes">
            <span class="scope-label">{t!("spotlight.mode_label")}</span>
            {pill("spatial", "spotlight.mode.spatial", !is_coeff, spatial_href)}
            {pill("coeff", "spotlight.mode.coeff", is_coeff, coeff_href)}
        </p>
    }
}

#[component]
fn NumField(label_key: &'static str, name: &'static str, value: f32) -> impl IntoView {
    let label = crate::i18n::translate(label_key, &crate::i18n::current_locale());
    view! {
        <label class="spotlight-num">
            <span class="lbl">{label}</span>
            <input
                type="number"
                name=name
                value=format!("{value}")
                min="0"
                max="1"
                step="0.05"
            />
        </label>
    }
}
