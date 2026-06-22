//! Stage 10: in-app documentation viewer.
//!
//! Renders the project's markdown docs (everything under `docs/`) into the
//! UI under `/help`. Mermaid diagrams and KaTeX math get post-processed
//! client-side; this module is responsible only for turning markdown into
//! HTML and selecting the right localized file.
//!
//! Localized variants live under `docs/<lang>/*.md` (en is the default at
//! `docs/*.md` for backwards-compat). Missing-locale fallback is English.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ssr")]
mod renderer;

/// Catalog of supported UI / docs locales. The order is the menu order.
pub const SUPPORTED_LOCALES: &[(&str, &str)] = &[
    ("en", "English"),
    ("ru", "Русский"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
];

/// `true` for any locale we ship docs / UI translations for. The HTTP
/// layer rejects unknown values to keep filesystem reads pinned to the
/// docs root.
pub fn is_supported_locale(l: &str) -> bool {
    SUPPORTED_LOCALES.iter().any(|(code, _)| *code == l)
}

/// Doc entry shown in the sidebar. `slug` is the URL-addressable id
/// (`api`, `architecture`, …) and matches the file stem under `docs/`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocItem {
    /// Stable identifier used in URLs and to load the file.
    pub slug: String,
    /// Display title, taken from the first `# heading` of the document.
    pub title: String,
}

/// Rendered doc returned to the page.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedDoc {
    /// Stable doc id.
    pub slug: String,
    /// Title scraped from the file's first H1.
    pub title: String,
    /// HTML body (already sanitized at parse time by `pulldown-cmark`).
    pub html: String,
    /// Locale this doc was actually served in (may differ from request
    /// when falling back to English).
    pub locale: String,
}

/// Canonical doc inventory. The slugs map 1:1 to file stems on disk so
/// adding a new doc means adding one entry here plus the `.md` files
/// under every `docs/<locale>/` directory.
pub fn doc_slugs() -> &'static [(&'static str, &'static str)] {
    &[
        ("README", "Documentation index"),
        ("architecture", "Architecture"),
        ("api", "API reference"),
        ("operations", "Operations guide"),
        ("theory", "Theory"),
        ("threat-model", "Threat model"),
        ("test-scenarios", "Test scenarios"),
    ]
}

/// List every doc visible in the sidebar for `locale`. The default
/// fallback title is the in-repo English heading; the SSR implementation
/// re-reads the H1 from disk so localized titles override it.
#[server(
    name = ListDocs,
    prefix = "/api",
    endpoint = "list_docs",
)]
pub async fn list_docs(locale: String) -> Result<Vec<DocItem>, ServerFnError> {
    if !is_supported_locale(&locale) {
        return Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
            "unsupported locale".into(),
        ));
    }
    let mut out = Vec::with_capacity(doc_slugs().len());
    for (slug, fallback_title) in doc_slugs() {
        let title = renderer::read_h1(&locale, slug)
            .unwrap_or_else(|| (*fallback_title).to_string());
        out.push(DocItem {
            slug: (*slug).to_string(),
            title,
        });
    }
    Ok(out)
}

/// Render a single doc to HTML. Falls back to English when the requested
/// locale has no file for `slug`. The returned `locale` field reflects
/// what was actually served.
#[server(
    name = GetDoc,
    prefix = "/api",
    endpoint = "get_doc",
)]
pub async fn get_doc(locale: String, slug: String) -> Result<RenderedDoc, ServerFnError> {
    if !is_supported_locale(&locale) {
        return Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
            "unsupported locale".into(),
        ));
    }
    if !doc_slugs().iter().any(|(s, _)| *s == slug) {
        return Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
            "unknown doc slug".into(),
        ));
    }
    let (served_locale, md) = renderer::read_doc(&locale, &slug).map_err(|e| {
        ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string())
    })?;
    let title = renderer::extract_h1(&md).unwrap_or_else(|| slug.clone());
    let html = renderer::render_markdown(&md);
    Ok(RenderedDoc {
        slug,
        title,
        html,
        locale: served_locale,
    })
}

// ===== Components ===========================================================

/// `/help` — landing page with the sidebar + README welcome doc.
#[component]
pub fn HelpIndexPage() -> impl IntoView {
    view! { <HelpLayout slug="README".to_string()/> }
}

/// `/help/:slug` — render one doc with the sidebar.
#[component]
pub fn HelpDocPage() -> impl IntoView {
    let params = leptos_router::hooks::use_params_map();
    let slug = move || params.with(|p| p.get("slug").unwrap_or_else(|| "README".to_string()));
    view! {
        {move || view! { <HelpLayout slug=slug()/> }}
    }
}

/// Shared layout: topbar nav + locale-aware sidebar + rendered doc body.
/// The Mermaid / KaTeX client-side scripts are loaded here so other pages
/// don't pay their weight. Locale comes from `?lang=` query (Stage 10
/// scaffold; the i18n layer in 10.2 supersedes this for UI strings).
#[component]
fn HelpLayout(slug: String) -> impl IntoView {
    let query = leptos_router::hooks::use_query_map();
    let locale_signal = move || {
        query.with(|q| {
            let l = q.get("lang").unwrap_or_else(|| "en".to_string());
            if is_supported_locale(&l) {
                l
            } else {
                "en".to_string()
            }
        })
    };

    let slug_for_resource = slug.clone();
    let doc_res = Resource::new(
        move || (locale_signal(), slug_for_resource.clone()),
        |(loc, slug)| async move { get_doc(loc, slug).await },
    );
    let toc_res = Resource::new(locale_signal, |loc| async move { list_docs(loc).await });

    let slug_for_sidebar = slug.clone();

    view! {
        <crate::ui::Topbar active="help"/>

        // === Client-side assets for Mermaid + KaTeX rendering. =============
        // Loaded only on /help routes so the catalog / health pages don't
        // pay the cost. We pull Mermaid and KaTeX from jsDelivr — vendoring
        // them (Mermaid alone is ~3 MB) into git is wasteful. The tiny
        // init shim lives under `assets/help/` and gets copied into
        // `target/site/assets/help/` by cargo-leptos at build time.
        <link
            rel="stylesheet"
            href="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.css"
        />
        <script
            defer=true
            src="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.js"
        ></script>
        <script
            defer=true
            src="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/contrib/auto-render.min.js"
        ></script>
        <script
            defer=true
            src="https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js"
        ></script>
        <script defer=true src="/assets/help/help-init.js"></script>

        <main class="container help-layout">
            <aside class="help-sidebar">
                <h3>{crate::t!("help.section.docs")}</h3>
                <Suspense fallback=move || view! { <p class="mut">"…"</p> }>
                    {move || {
                        let active = slug_for_sidebar.clone();
                        let loc = locale_signal();
                        toc_res.get().map(|res| match res {
                            Ok(items) => {
                                view! {
                                    <ul>
                                        <For
                                            each=move || items.clone()
                                            key=|i| i.slug.clone()
                                            children={
                                                let active = active.clone();
                                                let loc = loc.clone();
                                                move |item| {
                                                    let active_cls = if item.slug == active { "active" } else { "" };
                                                    let href = format!(
                                                        "/help/{}?lang={loc}",
                                                        crate::url_encode(&item.slug)
                                                    );
                                                    view! {
                                                        <li>
                                                            <a class=active_cls href=href>{item.title.clone()}</a>
                                                        </li>
                                                    }
                                                }
                                            }
                                        />
                                    </ul>
                                }.into_any()
                            }
                            Err(e) => view! {
                                <p class="bad">{e.to_string()}</p>
                            }.into_any(),
                        })
                    }}
                </Suspense>

                <h3 class="help-section-h">{crate::t!("help.section.language")}</h3>
                <ul class="help-langs">
                    {SUPPORTED_LOCALES.iter().map(|(code, label)| {
                        let cur_slug = slug.clone();
                        let href = format!(
                            "/help/{}?lang={code}",
                            crate::url_encode(&cur_slug)
                        );
                        view! {
                            <li>
                                <a href=href>{*label}</a>
                                " "
                                <span class="mut">"(" {*code} ")"</span>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </aside>

            <article class="help-doc">
                <Suspense fallback=move || view! { <p class="mut">{crate::t!("help.loading")}</p> }>
                    {move || doc_res.get().map(|res| match res {
                        Ok(doc) => view! {
                            <div class="help-meta mut">
                                "doc: " <code>{doc.slug.clone()}</code>
                                " · locale: " <code>{doc.locale.clone()}</code>
                            </div>
                            <div class="help-body" inner_html=doc.html></div>
                        }.into_any(),
                        Err(e) => view! {
                            <p class="bad">{crate::t!("help.load_failed")} " " {e.to_string()}</p>
                        }.into_any(),
                    })}
                </Suspense>
            </article>
        </main>
    }
}
