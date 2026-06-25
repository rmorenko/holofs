//! `/holo/<name>` — streaming-hologram demo.
//!
//! One full-bleed `<img>` whose `src` points to the
//! `multipart/x-mixed-replace` stream at `/preview/stream/<name>`.
//! The browser swaps the rendered pixels as each layer arrives — first
//! a heavily blurred L0 reconstruction shows up in ~50 ms, then the
//! image visibly *focuses* as L0-L1, L0-L2, … land. No JS — the
//! progression is driven entirely by the response body the server is
//! still writing.
//!
//! Sidebar lists the layer indices statically (the actual streaming
//! progress is reported via the `X-Holofs-Layer` header on each part,
//! which `<img>` doesn't surface to scripts; an inline `<details>` here
//! is the closest no-JS approximation of "show me what's loading").

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::t;

/// `GET /holo/<name>`.
#[component]
pub fn HoloPage() -> impl IntoView {
    let params = use_params_map();
    let name = move || params.with(|p| p.get("name").unwrap_or_default());

    view! {
        <crate::ui::Topbar active="catalog"/>
        <main class="container holo-page">
            {move || {
                let n = name();
                if n.is_empty() {
                    view! {
                        <p class="bad">{t!("holo.missing_name")}</p>
                        <p><a href="/" rel="external">{t!("generic.back_to_catalog")} " →"</a></p>
                    }.into_any()
                } else {
                    let enc = crate::url_encode(&n);
                    view! { <HoloBody name=n encoded=enc/> }.into_any()
                }
            }}
        </main>
    }
}

#[component]
fn HoloBody(name: String, encoded: String) -> impl IntoView {
    let stream_src = format!("/preview/stream/{encoded}");
    let full_href = format!("/{encoded}");
    let inspect_href = format!("/inspect/{encoded}");
    let name_for_title = name.clone();
    view! {
        <p class="mut holo-back">
            <a href="/" rel="external">"← " {t!("generic.back_to_catalog")}</a>
        </p>
        <h2 class="holo-h">
            {t!("holo.title_prefix")} " "
            <a href={full_href} rel="external">{name_for_title}</a>
        </h2>
        <p class="mut holo-intro">{t!("holo.intro")}</p>

        <div class="holo-stage">
            // The streaming img. Browser replaces the contents as each
            // multipart part arrives.
            <img
                class="holo-image"
                src=stream_src
                alt={name.clone()}
            />
        </div>

        <section class="holo-notes">
            <h3>{t!("holo.how_h")}</h3>
            <ol class="holo-steps">
                <li>{t!("holo.step1")}</li>
                <li>{t!("holo.step2")}</li>
                <li>{t!("holo.step3")}</li>
            </ol>
            <p class="mut">
                {t!("holo.see_also")} " "
                <a href={inspect_href} rel="external">{t!("holo.see_shards")}</a>
                " · "
                <a href="/about" rel="external">{t!("holo.see_about")}</a>
            </p>
        </section>
    }
}
