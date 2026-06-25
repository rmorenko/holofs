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
    // Stage 11.14: preserve `?lang=<code>` across nav clicks. Without
    // this, clicking "catalog" from `/health?lang=ru` would drop the
    // locale and bounce the user back to English. Helper closure reads
    // the live locale signal and appends the param when it isn't `en`.
    let nav_href = move |base: &'static str| {
        let lang = current_locale();
        if lang == "en" {
            base.to_string()
        } else {
            format!("{base}?lang={lang}")
        }
    };
    view! {
        <header class="topbar">
            <h1>"holofs"</h1>
            <nav>
                <a href=move || nav_href("/") class=cls_for("catalog")>
                    {move || translate("nav.catalog", &current_locale())}
                </a>
                <a href=move || nav_href("/search") class=cls_for("search")>
                    {move || translate("nav.search", &current_locale())}
                </a>
                <a href=move || nav_href("/health") class=cls_for("health")>
                    {move || translate("nav.health", &current_locale())}
                </a>
                <a href=move || nav_href("/escrow") class=cls_for("escrow")>
                    {move || translate("nav.escrow", &current_locale())}
                </a>
                <a href=move || nav_href("/help") class=cls_for("help")>
                    {move || translate("nav.help", &current_locale())}
                </a>
                <a href=move || nav_href("/about") class=cls_for("about")>
                    {move || translate("nav.about", &current_locale())}
                </a>
                <LocaleSwitcher/>
                <ThemeToggle/>
            </nav>
        </header>
    }
}

/// Stage 11.13: light / dark theme toggle. Pure inline JS — clicks flip
/// `data-theme` on `<html>` and persist the choice in localStorage so the
/// pre-paint script in `App` picks it up on the next render. Without JS
/// the button just doesn't do anything; the static markup stays
/// readable in whichever theme the server happened to default to.
#[component]
pub fn ThemeToggle() -> impl IntoView {
    let onclick = "var d=document.documentElement;\
        var cur=d.dataset.theme||'dark';\
        var next=cur==='dark'?'light':'dark';\
        d.dataset.theme=next;\
        try{localStorage.setItem('holofs-theme',next);}catch(e){}";
    view! {
        <button
            type="button"
            class="theme-toggle"
            onclick=onclick
            title={move || translate("theme.toggle", &current_locale())}
        >
            "◐"
        </button>
    }
}
