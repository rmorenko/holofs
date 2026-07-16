//! holofs-web: Leptos SSR + CSR shared crate.
//!
//! `src/lib.rs` is built twice:
//! - under the `ssr` feature on the server (linked into `holofs-web` binary
//!   via axum + leptos_axum);
//! - under the `hydrate` feature for the browser (compiled to WASM and
//!   shipped as `pkg/holofs.{js,wasm}`).
//!
//! Shared types (e.g. [`CatalogEntry`]) and components live here so the same
//! Rust code drives the server render and the client hydrate.

use leptos::prelude::*;
use leptos_meta::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::use_query_map;
use leptos_router::path;
use serde::{Deserialize, Serialize};

pub mod about;
#[cfg(feature = "ssr")]
pub mod admin_auth;
#[cfg(feature = "ssr")]
pub mod backpressure;
pub mod bootstrap;
pub mod catalog_types;
pub mod catalog_ui;
#[cfg(feature = "ssr")]
pub mod cli;
#[cfg(feature = "ssr")]
pub mod config_file;
pub mod diff;
pub mod escrow;
#[cfg(feature = "ssr")]
pub mod filter;
#[cfg(feature = "ssr")]
pub mod handlers;
pub mod health;
pub mod help;
pub mod holo;
pub mod i18n;
pub mod inspect;
pub mod mix;
#[cfg(feature = "ssr")]
pub mod range;
#[cfg(feature = "ssr")]
pub mod rate_limit;
#[cfg(feature = "ssr")]
pub mod runtime_config;
pub mod search;
pub mod server_fns;
pub mod similar;
pub mod spotlight;
#[cfg(feature = "ssr")]
pub mod supervised;
#[cfg(feature = "ssr")]
pub mod timeout;
pub mod ui;
pub mod versions;

// Re-export the public catalog view-model + server function types at
// the crate root so the historical `holofs_web::CatalogEntry` /
// `holofs_web::GetCatalog` / `holofs_web::ListDir` /
// `holofs_web::ListDirPageFn` paths keep resolving.
pub use catalog_types::CatalogEntry;
pub use server_fns::{
    get_catalog, list_dir, list_dir_page, GetCatalog, ListDir, ListDirPage, ListDirPageFn,
};
pub(crate) use server_fns::TreeSort;
#[cfg(feature = "ssr")]
pub(crate) use server_fns::compare_entries;


/// Root component. Renders the full HTML document; in Leptos 0.6 the App
/// owns the `<html>`/`<head>`/`<body>` shell.
///
/// added i18n: the active locale is provided as
/// [`i18n::LocaleSignal`] via Leptos context here so every nested
/// component can call `t!("key")` without threading it through props.
/// Locale source is the `?lang=<code>` query string on the current URL;
/// cookie / `Accept-Language` based persistence is deferred to a future
/// pass that needs the axum request to flow into `provide_context`.
/// Document shell — `<!DOCTYPE html>` through `</html>`. Owns the
/// `<head>` and embeds `<App/>` inside `<body>`. Leptos 0.7's
/// `render_app_to_stream` family expects the whole document to come
/// from the rendered view, with `<HydrationScripts/>` inside `<head>`
/// to bootstrap the WASM bundle on the client. Keeping the shell
/// separate from `App` avoids accidentally double-wrapping the output
/// in nested `<html><head><body>` trees.
#[component]
pub fn Shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                // + 11.26: pre-paint bootstrap. Restores the
                // user's last theme (data-theme attribute on <html>) and
                // the persisted tree zoom (--tree-scale CSS var) before
                // any styles paint — avoids a flash of the wrong size
                // or palette on reload.
                <script>{
                    "(function(){try{\
                    var r=document.documentElement;\
                    var t=localStorage.getItem('holofs-theme')||'dark';r.dataset.theme=t;\
                    var s=localStorage.getItem('holofs-tree-scale');\
                    if(s)r.style.setProperty('--tree-scale',s);\
                    }catch(e){}})();"
                }</script>
                // shared expand / collapse helpers used by
                // the controls bar (global expand-all) and the per-folder
                // ⊕ / ⊖ buttons. Expand keeps retrying for a beat after
                // it stops making progress so lazily-mounted children get
                // picked up — the LazyDirNode toggle handler fires a
                // fetch on open, the fresh `<details>` only appear once
                // that fetch resolves and renders, so a single querySelectorAll
                // sweep would miss them.
                <script>{
                    // /28: recursive expand. We keep retrying
                    // until no new <details> show up for ~5 seconds in
                    // a row — that's the slack the deepest lazy fetches
                    // need to materialise their inner LazyDirNodes
                    // before we declare the tree fully open. Earlier
                    // value (8 × 200ms = 1.6s) wasn't enough for chains
                    // 4-5 levels deep on a busy cluster.
                    //
                    // fix (user-reported "клик ⊖ blinks
                    // and re-opens"): a pending step() from the user's
                    // earlier ⊕ click would race a later ⊖ click,
                    // find descendants with open=false, and forcibly
                    // re-open them via `if(!d.open){d.open=true}`.
                    // Use a global epoch counter that BOTH helpers
                    // bump on entry; every queued step() checks the
                    // epoch it captured and exits if a newer call
                    // (expand or collapse) has since started. The
                    // root-is-DETAILS check stays as a fast path so
                    // a closed-from-summary-click also aborts.
                    "(function(){var ep=0;\
                    window.holofsExpandAll=function(root){\
                    if(!root)return;ep++;var my=ep,tries=25;function step(){\
                    if(my!==ep)return;\
                    if(root.tagName==='DETAILS'&&!root.open)return;\
                    var any=false;\
                    root.querySelectorAll('details').forEach(function(d){\
                    if(!d.open){d.open=true;any=true;}});\
                    if(any){tries=25;setTimeout(step,200);}\
                    else if(--tries>0){setTimeout(step,200);}\
                    }if(root.tagName==='DETAILS')root.open=true;step();};\
                    window.holofsCollapseAll=function(root){if(!root)return;ep++;\
                    if(root.tagName==='DETAILS')root.open=false;\
                    root.querySelectorAll('details').forEach(function(d){d.open=false;});};\
                    })();"
                }</script>
                <HydrationScripts options/>
                // catalog-tree sticky H-scrollbar
                // initialiser. Lives in the static head so it runs
                // on every page; if the tree isn't on the current
                // page the script is a no-op (init finds no
                // `.tree-scroll` element).
                <script defer="defer" src="/assets/tree-hscroll.js"></script>
                // global "something is loading" indicator
                // — patches fetch/XHR/form-submit so 95 % of the
                // async UI gets a top-of-viewport progress bar
                // for free. See `assets/busy-indicator.js` for the
                // counter-based gate + `<form>` auto-disable.
                <script defer="defer" src="/assets/busy-indicator.js"></script>
                // progressive-enhancement layer for mutation
                // forms: intercept submit, POST via fetch/XHR, toast
                // on success/error inside the current layout. See
                // `assets/mutation-forms.js` for the opt-in
                // `data-holofs-mutate="1"` protocol. Closes A1 / A2
                // from UI-UX-review.md.
                <script defer="defer" src="/assets/mutation-forms.js"></script>
                // Auto-enhances every upload form on the page (styled
                // focus UploadForm + bare tree-inline / tree-root
                // uploads) with drag-drop and filename preview.
                // Was mounted only inside UploadForm — leaving the
                // tree variants stuck on the bare `<input>` UX. See
                // `assets/upload-init.js`.
                <script defer="defer" src="/assets/upload-init.js"></script>
                <Stylesheet id="leptos" href="/pkg/holofs.css"/>
                <Title text="holofs"/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <RoutedApp/>
        </Router>
    }
}

/// Lives inside `<Router>` so `use_query_map` is callable. Provides the
/// i18n locale context (driven by `?lang=`) and mounts the Routes table.
#[component]
fn RoutedApp() -> impl IntoView {
    use leptos_router::hooks::use_query_map;

    // Reactive locale from `?lang=`. Defaults to `"en"`; unknown codes
    // also fall back so a hand-edited URL can't break the page.
    let query = use_query_map();
    let locale = Memo::new(move |_| {
        query.with(|q| {
            q.get("lang")
                .filter(|l| i18n::is_known_locale(l))
                .unwrap_or_else(|| "en".to_string())
        })
    });
    provide_context(i18n::LocaleSignal(locale.into()));

    view! {
        <Routes fallback=|| view! { <p>"not found"</p> }>
            <Route path=path!("/") view=catalog_ui::CatalogPage/>
            <Route path=path!("/health") view=health::HealthIndexPage/>
            <Route path=path!("/health/*name") view=health::HealthDetailPage/>
            // zoom puts the fixed-format slot in front of the
            // wildcard path so leptos_router accepts the trailing splat.
            <Route path=path!("/inspect-zoom/:c_l_idx/*name") view=inspect::InspectZoomPage/>
            <Route path=path!("/inspect/*name") view=inspect::InspectPage/>
            <Route path=path!("/similar/*name") view=similar::SimilarPage/>
            // two object paths don't fit a single routable
            // pattern; diff reads them from the query.
            <Route path=path!("/diff") view=diff::DiffPage/>
            // wavelet-mix composer.
            <Route path=path!("/mix") view=mix::MixPage/>
            <Route path=path!("/escrow") view=escrow::EscrowPage/>
            <Route path=path!("/help") view=help::HelpIndexPage/>
            <Route path=path!("/help/:slug") view=help::HelpDocPage/>
            <Route path=path!("/about") view=about::AboutPage/>
            <Route path=path!("/search") view=search::SearchPage/>
            <Route path=path!("/holo/*name") view=holo::HoloPage/>
            <Route path=path!("/spotlight") view=spotlight::SpotlightPage/>
            <Route path=path!("/versions/*name") view=versions::VersionsPage/>
        </Routes>
    }
}

/// Tiny URL encoder — only escapes the characters that break a path
/// segment in a browser address bar. added `/` to the allow list
/// so multi-segment catalog paths render as `/a/b/c` rather than
/// `/a%2Fb%2Fc`. Good enough for object paths; the legacy gateway uses
/// the same encoding style.
pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

/// WASM entry point. cargo-leptos generates the JS glue that calls this.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}

