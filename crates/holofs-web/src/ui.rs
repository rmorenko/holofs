//! Shared UI primitives used across multiple Leptos pages.
//!
//! Stage 10 carved this module out so the topbar (page navigation +
//! locale switcher) lives in one place — every page that used to copy the
//! `<header class="topbar">` block now embeds `<Topbar active="…"/>` and
//! gets translations + the locale picker for free.

use leptos::prelude::*;

use crate::i18n::{current_locale, translate, LocaleSwitcher};

/// Persistent top-of-page nav. `active` is the slug of the section the
/// current page belongs to (`"catalog"`, `"health"`, `"escrow"`,
/// `"help"`), used to highlight the right link.
#[component]
pub fn Topbar(active: &'static str) -> impl IntoView {
    let cls_for = move |slug: &'static str| {
        if slug == active {
            "active"
        } else {
            ""
        }
    };
    view! {
        <header class="topbar">
            <h1>"holofs"</h1>
            <nav>
                <a href="/" class=cls_for("catalog")>
                    {move || translate("nav.catalog", &current_locale())}
                </a>
                <a href="/health" class=cls_for("health")>
                    {move || translate("nav.health", &current_locale())}
                </a>
                <a href="/escrow" class=cls_for("escrow")>
                    {move || translate("nav.escrow", &current_locale())}
                </a>
                <a href="/help" class=cls_for("help")>
                    {move || translate("nav.help", &current_locale())}
                </a>
                <LocaleSwitcher/>
            </nav>
        </header>
    }
}
