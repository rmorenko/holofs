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

#[cfg(feature = "ssr")]
pub mod bootstrap;
#[cfg(feature = "ssr")]
pub mod cli;
pub mod diff;
pub mod escrow;
#[cfg(feature = "ssr")]
pub mod handlers;
pub mod health;
pub mod help;
pub mod i18n;
pub mod inspect;
#[cfg(feature = "ssr")]
pub mod range;
pub mod similar;
pub mod ui;

/// Catalog view-model carried over the wire by the [`get_catalog`] server
/// function. Stays plain-serde so both SSR and hydrate compile it cleanly
/// (no tokio / no holofs-gateway dependency).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogEntry {
    pub name: String,
    /// One of `"image"`, `"audio"`, `"text"`, `"opaque"`.
    pub kind: String,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
    pub n_shards: u32,
    pub cid_short: String,
    pub audio_sample_rate: u32,
    pub channels: u8,
}

/// Catalog snapshot for `GET /`. Reads the live `Gateway` from leptos
/// context; under hydrate the macro emits a client stub that POSTs to
/// `/api/get_catalog`. `endpoint = "get_catalog"` pins the URL — without it
/// the macro appends a hash to the path.
#[server(
    name = GetCatalog,
    prefix = "/api",
    endpoint = "get_catalog",
)]
pub async fn get_catalog() -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let catalog = gw.catalog().lock().await;
    let mut out: Vec<CatalogEntry> = catalog
        .entries
        .iter()
        .map(|(name, m)| CatalogEntry::from_manifest(name, m))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `GET /api/list_dir?prefix=...` — children one level under `prefix`. Pass
/// an empty string for the root listing. Errors map 1:1 to
/// [`GatewayError`]: 404 if the prefix is unknown, 409 if it's a real
/// object, 400 if it's malformed.
#[server(
    name = ListDir,
    prefix = "/api",
    endpoint = "list_dir",
)]
pub async fn list_dir(prefix: String) -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let children = gw
        .list_dir(&prefix)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let mut out: Vec<CatalogEntry> = children
        .into_iter()
        .map(|(name, m)| CatalogEntry::from_manifest(&name, &m))
        .collect();
    // Directories first, then objects — both alphabetical inside the bucket.
    out.sort_by(|a, b| {
        let a_dir = a.kind == "directory";
        let b_dir = b.kind == "directory";
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

#[cfg(feature = "ssr")]
impl CatalogEntry {
    /// Project a `Manifest` into the wire-friendly view-model.
    pub(crate) fn from_manifest(name: &str, m: &holofs_model::manifest::Manifest) -> Self {
        use holofs_core::hash::hex;
        use holofs_model::manifest::ObjectKind;

        let kind = match m.kind {
            ObjectKind::Image => "image",
            ObjectKind::Audio => "audio",
            ObjectKind::Text => "text",
            ObjectKind::Opaque => "opaque",
            ObjectKind::Directory => "directory",
        }
        .to_string();
        let total_shards: u32 = m.n_per_layer.iter().sum::<u32>() * u32::from(m.channels);
        let cid_full = hex(&m.data_cid);
        Self {
            name: name.to_string(),
            kind,
            content_type: m.content_type.clone(),
            width: m.width,
            height: m.height,
            n_shards: total_shards,
            cid_short: cid_full.chars().take(12).collect(),
            audio_sample_rate: m.audio_sample_rate,
            channels: m.channels,
        }
    }
}

/// Root component. Renders the full HTML document; in Leptos 0.6 the App
/// owns the `<html>`/`<head>`/`<body>` shell.
///
/// Stage 10 added i18n: the active locale is provided as
/// [`i18n::LocaleSignal`] via Leptos context here so every nested
/// component can call `t!("key")` without threading it through props.
/// Locale source is the `?lang=<code>` query string on the current URL;
/// cookie / `Accept-Language` based persistence is deferred to a future
/// pass that needs the axum request to flow into `provide_context`.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <Stylesheet id="leptos" href="/pkg/holofs.css"/>
                <Title text="holofs"/>
            </head>
            <body>
                <Router>
                    <RoutedApp/>
                </Router>
            </body>
        </html>
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
            <Route path=path!("/") view=CatalogPage/>
            <Route path=path!("/health") view=health::HealthIndexPage/>
            <Route path=path!("/health/*name") view=health::HealthDetailPage/>
            // Stage 9: zoom puts the fixed-format slot in front of the
            // wildcard path so leptos_router accepts the trailing splat.
            <Route path=path!("/inspect-zoom/:c_l_idx/*name") view=inspect::InspectZoomPage/>
            <Route path=path!("/inspect/*name") view=inspect::InspectPage/>
            <Route path=path!("/similar/*name") view=similar::SimilarPage/>
            // Stage 9: two object paths don't fit a single routable
            // pattern; diff reads them from the query.
            <Route path=path!("/diff") view=diff::DiffPage/>
            <Route path=path!("/escrow") view=escrow::EscrowPage/>
            <Route path=path!("/help") view=help::HelpIndexPage/>
            <Route path=path!("/help/:slug") view=help::HelpDocPage/>
        </Routes>
    }
}

/// `GET /` — hierarchical catalog view. `?p=<path>` selects the directory
/// to list; the empty (or absent) value lists the root. Stage 9 made the
/// catalog tree-shaped; this page is the entry point into it.
#[component]
fn CatalogPage() -> impl IntoView {
    let query = use_query_map();
    let prefix_signal = move || query.with(|q| q.get("p").unwrap_or_default());
    let entries = Resource::new(prefix_signal, |p| async move { list_dir(p).await });

    view! {
        <ui::Topbar active="catalog"/>

        <main class="container">
            {move || {
                let prefix = prefix_signal();
                view! { <Breadcrumb prefix=prefix.clone()/> }
            }}

            {move || {
                let prefix = prefix_signal();
                view! { <UploadForm parent=prefix.clone()/> }
            }}

            {move || {
                let prefix = prefix_signal();
                view! { <MkdirForm parent=prefix.clone()/> }
            }}

            <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
                {move || {
                    let prefix = prefix_signal();
                    entries.get().map(|res| match res {
                        Ok(list) if list.is_empty() => view! {
                            <p class="empty-state">
                                {t!("catalog.empty")} " " {t!("catalog.empty_hint")} " "
                                <code>{format!("curl -X PUT http://<host>/{prefix}/<name>")}</code>
                            </p>
                        }.into_any(),
                        Ok(list) => {
                            let parent_for_cards = prefix.clone();
                            view! {
                                <div class="grid">
                                    <For
                                        each=move || list.clone()
                                        key=|e| e.name.clone()
                                        children={
                                            let parent = parent_for_cards.clone();
                                            move |e| view! {
                                                <ObjectCard entry=e parent=parent.clone()/>
                                            }
                                        }
                                    />
                                </div>
                            }.into_any()
                        },
                        Err(e) => view! {
                            <p class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</p>
                        }.into_any(),
                    })
                }}
            </Suspense>
        </main>
    }
}

/// Breadcrumb trail: `home / a / b / c`. Each ancestor segment links back
/// to its own catalog view; the last segment is plain text.
#[component]
fn Breadcrumb(prefix: String) -> impl IntoView {
    if prefix.is_empty() {
        return view! {
            <nav class="breadcrumb"><strong>{t!("breadcrumb.home")}</strong></nav>
        }
        .into_any();
    }
    let segs: Vec<&str> = prefix.split('/').collect();
    let mut trail: Vec<(String, String)> = Vec::with_capacity(segs.len());
    let mut so_far = String::new();
    for s in &segs {
        if !so_far.is_empty() {
            so_far.push('/');
        }
        so_far.push_str(s);
        trail.push(((*s).to_string(), so_far.clone()));
    }
    let last_idx = trail.len() - 1;
    view! {
        <nav class="breadcrumb">
            <a href="/">{t!("breadcrumb.home")}</a>
            {trail.into_iter().enumerate().map(|(i, (seg, full))| {
                if i == last_idx {
                    view! {
                        <span>" / "</span>
                        <strong>{seg}</strong>
                    }.into_any()
                } else {
                    let enc = url_encode(&full);
                    view! {
                        <span>" / "</span>
                        <a href={format!("/?p={enc}")}>{seg}</a>
                    }.into_any()
                }
            }).collect_view()}
        </nav>
    }
    .into_any()
}

/// Inline "new folder" form. Posts to `/api/mkdir` with `parent` + `name`
/// fields; on success the server 303-redirects back to the current
/// directory so the new tile appears immediately.
#[component]
fn MkdirForm(parent: String) -> impl IntoView {
    let placeholder = move || t!("mkdir.placeholder");
    view! {
        <form class="mkdir-form" method="POST" action="/api/mkdir">
            <input type="hidden" name="parent" value=parent/>
            <input
                type="text"
                name="name"
                placeholder=placeholder
                required=true
                minlength="1"
            />
            <button type="submit">{t!("mkdir.submit")}</button>
        </form>
    }
}

/// Inline file-upload form. Posts to `/api/upload` (multipart) with the
/// destination directory baked in as a hidden field; on success the
/// server 303-redirects back to `/?p=<parent>` so the new tile shows up.
///
/// Stage 11.4 polish: the native `<input type="file">` is visually-hidden;
/// a styled `<label>` takes its place as the click target, and a sibling
/// `<span>` shows the chosen filename (updated by `upload-init.js`).
/// The same script wires drag-and-drop on the surrounding `.upload-form`
/// box so files dropped anywhere on the dashed area land on the input.
/// Without JS the form still works — the label acts as a button natively.
#[component]
fn UploadForm(parent: String) -> impl IntoView {
    let name_ph = move || t!("upload.name_placeholder");
    view! {
        <section class="upload-form">
            <p class="hint">{t!("upload.hint")}</p>
            <p class="drop-prompt mut">{t!("upload.drop_here")}</p>
            <form method="POST" action="/api/upload" enctype="multipart/form-data">
                <input type="hidden" name="parent" value=parent/>
                <label class="file-label">
                    <input type="file" name="file" required=true/>
                    <span class="file-button">{t!("upload.choose_file")}</span>
                    <span class="file-name mut">{t!("upload.no_file")}</span>
                </label>
                <input
                    type="text"
                    name="name"
                    placeholder=name_ph
                />
                <button type="submit">{t!("upload.submit")}</button>
            </form>
            <script defer="defer" src="/assets/upload-init.js"></script>
        </section>
    }
}

/// One card in the catalog grid. Visual structure mirrors the legacy
/// gateway's HTML: thumb on top, metadata table, actions row. Stage 9
/// added the `parent` prop so the basename ("img.jpg" out of
/// "photos/2026/img.jpg") shows in the tile while every link still uses
/// the full path.
#[component]
fn ObjectCard(entry: CatalogEntry, parent: String) -> impl IntoView {
    let CatalogEntry {
        name,
        kind,
        content_type,
        width,
        height,
        n_shards,
        cid_short,
        audio_sample_rate,
        channels,
    } = entry;

    let basename = if parent.is_empty() {
        name.clone()
    } else {
        let cut = parent.len() + 1;
        name.get(cut..).unwrap_or(&name).to_string()
    };
    let enc_full = url_encode(&name);

    if kind == "directory" {
        let confirm_js = format!(
            "return confirm('{}');",
            t!("folder.confirm_delete").replace('\'', "\\'")
        );
        return view! {
            <article class="card kind-directory">
                <a class="thumb dir-thumb" href={format!("/?p={enc_full}")}>
                    <span class="icon">"📁"</span>
                </a>
                <div class="meta">
                    <div class="name">
                        <a href={format!("/?p={enc_full}")}>{basename.clone()}</a>
                    </div>
                    <div class="row mut">{t!("folder.kind_label")}</div>
                </div>
                <div class="actions">
                    <a href={format!("/?p={}", enc_full.clone())}>{t!("folder.open")}</a>
                    " · "
                    <form
                        method="POST"
                        action="/api/rmdir"
                        class="inline-form"
                        onsubmit=confirm_js
                    >
                        <input type="hidden" name="path" value=name.clone()/>
                        <button type="submit" class="link-btn">{t!("folder.delete")}</button>
                    </form>
                </div>
            </article>
        }.into_any();
    }

    let dims = match kind.as_str() {
        "image" => format!("{width}×{height}"),
        "audio" => format!("{audio_sample_rate} Hz · {channels} ch"),
        _ => content_type.clone(),
    };

    view! {
        <article class={format!("card kind-{kind}")}>
            <div class="thumb">
                {match kind.as_str() {
                    "image" => view! { <img src={format!("/preview/{enc_full}")} loading="lazy" alt=""/> }.into_any(),
                    "audio" => view! { <span class="icon">"♪"</span> }.into_any(),
                    "text"  => view! { <span class="icon">"¶"</span> }.into_any(),
                    _       => view! { <span class="icon">"📦"</span> }.into_any(),
                }}
            </div>
            <div class="meta">
                <div class="name">{basename.clone()}</div>
                <div class="row mut">{dims}</div>
                <div class="row mut">{n_shards} " " {t!("card.shards")}</div>
                <div class="row mut cid"><code>{cid_short}</code></div>
            </div>
            <div class="actions">
                <a href={format!("/{enc_full}")}>{primary_label(&kind)}</a>
                {(kind == "image" || kind == "audio").then(|| view! {
                    " · " <a href={format!("/preview/{}", enc_full.clone())}>{preview_label(&kind)}</a>
                })}
                " · " <a href={format!("/inspect/{}", enc_full.clone())}>{t!("card.action.shards_link")}</a>
                " · " <a href={format!("/similar/{}", enc_full.clone())}>{t!("card.action.similar")}</a>
                " · " <a href={format!("/health/{}", enc_full.clone())}>{t!("card.action.health")}</a>
            </div>
        </article>
    }
    .into_any()
}

fn primary_label(kind: &str) -> &'static str {
    let loc = i18n::current_locale();
    match kind {
        "image" => i18n::translate("card.action.full", &loc),
        "audio" => i18n::translate("card.action.play", &loc),
        "text" => i18n::translate("card.action.open", &loc),
        _ => i18n::translate("card.action.download", &loc),
    }
}

fn preview_label(kind: &str) -> &'static str {
    let loc = i18n::current_locale();
    match kind {
        "image" => i18n::translate("card.action.preview", &loc),
        "audio" => i18n::translate("card.action.bass", &loc),
        _ => i18n::translate("card.action.preview", &loc),
    }
}

/// Tiny URL encoder — only escapes the characters that break a path
/// segment in a browser address bar. Stage 9 added `/` to the allow list
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
