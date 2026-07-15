//! Leptos components that make up the catalog page and its tree view.
//!
//! Fifteen `#[component]` blocks live here:
//!
//! - **Entry points.** [`CatalogPage`] is the `/` route dispatcher —
//!   either the tree view (root path) or the focus view (`?p=`).
//!   [`CatalogFocusView`] renders one directory with breadcrumb +
//!   upload + mkdir + tile grid.
//! - **Controls.** [`FilterBar`], [`TreeZoomButtons`], [`Breadcrumb`].
//! - **Tree rendering.** [`CatalogTreeView`] switches between the
//!   eager filter-active tree ([`CatalogTreeEager`]) and the lazy
//!   default tree ([`CatalogTreeLazy`] + [`CatalogTreeLazyShell`] +
//!   [`LazyLevel`] + [`LazyDirNode`]). The eager path uses
//!   [`CatalogTreeBody`] + [`TreeNodeView`] to fold a flat
//!   [`CatalogEntry`] vec into a nested `<details>` DOM.
//! - **Forms.** [`MkdirForm`] and [`UploadForm`] post to the axum
//!   handlers with a 303-back-to-`?p=` redirect on success.
//! - **Tiles.** [`ObjectCard`] renders one row of the focus view
//!   with per-kind action links.
//!
//! Every server-side pagination call (`list_dir_page` /
//! `list_dir` / `get_catalog`) lives in [`crate::server_fns`]; the
//! filter helpers live in [`crate::filter`]. This module owns UI
//! only.
//!

use leptos::prelude::*;
use leptos_meta::*;
use leptos_router::hooks::use_query_map;

use crate::catalog_types::CatalogEntry;
use crate::i18n;
use crate::server_fns::{get_catalog, list_dir, list_dir_page, ListDirPage, TreeSort};
use crate::t;
use crate::ui;
use crate::url_encode;

/// `GET /` — catalog entry point. Two modes:
///
/// - `/` (no `?p=`): **tree-view** of the entire catalog with
///   collapsible `<details>` nodes. Read-only — no upload / mkdir forms.
///   Each folder header carries an `[open →]` link to the focus view.
/// - `/?p=<path>`: **focus view** of a single directory with breadcrumb,
///   upload + mkdir forms and per-tile actions. This is where creates
///   and deletes happen.
#[component]
pub fn CatalogPage() -> impl IntoView {
    use leptos_router::hooks::use_query_map;
    let query = use_query_map();
    view! {
        // tag `<body>` so the catalog page can opt into
        // `body { overflow: hidden; height: 100vh }` — kills the
        // window-level vertical scroll. Other routes (help, similar
        // etc.) without this class keep normal page scrolling.
        <Body attr:class="catalog-page"/>
        <ui::Topbar active="catalog"/>

        <main class="container">
            {move || {
                // The catalog is one tree, but its ROOT can be any
                // folder: `/?p=<path>` makes <path>'s children the
                // top-level entries (proper "cd into folder"; "open"
                // on a card lands here). `?open=` is kept as an
                // alias for backward-compat with old bookmarks.
                let prefix = query.with(|q| {
                    q.get("p")
                        .or_else(|| q.get("open"))
                        .unwrap_or_default()
                });
                view! { <CatalogTreeView prefix=prefix/> }.into_any()
            }}
        </main>
    }
}

/// Focused single-directory view: breadcrumb + upload + mkdir + tile
/// grid. Reachable via `/?p=<path>` — this is where catalog mutations
/// happen.
#[component]
fn CatalogFocusView(prefix: String) -> impl IntoView {
    let query = use_query_map();
    let filter_signal = move || {
        query.with(|q| {
            (
                q.get("q").unwrap_or_default(),
                q.get("from").unwrap_or_default(),
                q.get("to").unwrap_or_default(),
            )
        })
    };
    let prefix_clone = prefix.clone();
    let entries = Resource::new(
        move || (prefix_clone.clone(), filter_signal()),
        |(p, (q, f, t))| async move { list_dir(p, q, f, t).await },
    );
    let prefix_for_show = prefix.clone();
    let filter_prefix = prefix.clone();

    view! {
        <Breadcrumb prefix=prefix.clone()/>
        <UploadForm parent=prefix.clone()/>
        <MkdirForm parent=prefix.clone()/>
        <FilterBar prefix=filter_prefix/>
        <p class="mut focus-tip">
            <a href="/">"← " {t!("catalog.back_to_tree")}</a>
        </p>

        <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
            {move || {
                let prefix = prefix_for_show.clone();
                entries.get().map(|res| match res {
                    Ok(list) if list.is_empty() => {
                        // A5: distinguish "empty directory" (true empty)
                        // from "filter matched nothing" (user should know
                        // to relax the filter, not curl-PUT).
                        let (q, f, t) = filter_signal();
                        let filter_on = !q.is_empty() || !f.is_empty() || !t.is_empty();
                        let clear_href = if prefix.is_empty() {
                            "/".to_string()
                        } else {
                            format!("/?p={prefix}")
                        };
                        if filter_on {
                            view! {
                                <p class="empty-state">
                                    {t!("catalog.no_results_for_filter")} " "
                                    <a href=clear_href>{t!("catalog.clear_filter")}</a>
                                </p>
                            }.into_any()
                        } else {
                            view! {
                                <p class="empty-state">
                                    {t!("catalog.empty")} " " {t!("catalog.empty_hint")} " "
                                    <code>{format!("curl -X PUT http://<host>/{prefix}/<name>")}</code>
                                </p>
                            }.into_any()
                        }
                    },
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
    }
}

/// shared `−` / `+` zoom buttons for the tree controls
/// bar. Stores the scale in `localStorage` under `holofs-tree-scale`
/// and updates the `--tree-scale` CSS variable on `<html>`, which
/// every tree font-size / spacing rule multiplies into via `calc()`.
/// A pre-paint script in the `Shell` reads the stored value before
/// the first paint so reloads keep the user's zoom.
#[component]
fn TreeZoomButtons() -> impl IntoView {
    let onclick_in = "(function(s){var r=document.documentElement;\
        var c=parseFloat(localStorage.getItem('holofs-tree-scale')||'1')||1;\
        var n=Math.min(2,Math.max(0.5,Math.round((c+s)*10)/10));\
        r.style.setProperty('--tree-scale',n);\
        try{localStorage.setItem('holofs-tree-scale',n);}catch(e){}})(0.1)";
    let onclick_out = "(function(s){var r=document.documentElement;\
        var c=parseFloat(localStorage.getItem('holofs-tree-scale')||'1')||1;\
        var n=Math.min(2,Math.max(0.5,Math.round((c+s)*10)/10));\
        r.style.setProperty('--tree-scale',n);\
        try{localStorage.setItem('holofs-tree-scale',n);}catch(e){}})(-0.1)";
    view! {
        <button
            type="button"
            class="tree-control-btn tree-control-zoom"
            onclick=onclick_out
            title={t!("tree.zoom_out")}
        >
            <span class="tree-control-icon">"−"</span>
        </button>
        <button
            type="button"
            class="tree-control-btn tree-control-zoom"
            onclick=onclick_in
            title={t!("tree.zoom_in")}
        >
            <span class="tree-control-icon">"+"</span>
        </button>
    }
}

/// server-side catalog filter bar. Renders a GET form that
/// submits `?q=&from=&to=` (plus the current `?p=<prefix>` for focus
/// mode) so the page reloads with a filtered listing. The submit target
/// is the same path the user is on, which preserves the focus / tree
/// mode toggle. Submitting an empty form clears every filter — that's
/// the same as visiting `/` without any query string.
#[component]
fn FilterBar(prefix: String) -> impl IntoView {
    let query = use_query_map();
    let (q_init, from_init, to_init) = query.with(|q| {
        (
            q.get("q").unwrap_or_default(),
            q.get("from").unwrap_or_default(),
            q.get("to").unwrap_or_default(),
        )
    });
    let has_filter = !q_init.is_empty() || !from_init.is_empty() || !to_init.is_empty();
    let prefix_for_hidden = prefix.clone();
    view! {
        // progressive enhancement — flatpickr replaces the
        // native date picker with a cross-browser one whose UI follows
        // the page locale. We pull the lib + the 4 non-English locale
        // bundles from jsdelivr (same CDN pattern as the help page's
        // KaTeX/Mermaid). Without JS / on CDN failure, the native
        // `<input type="date">` keeps working.
        <link
            rel="stylesheet"
            href="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/flatpickr.min.css"
        />
        <script
            defer="defer"
            src="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/flatpickr.min.js"
        ></script>
        <script defer="defer" src="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/ru.js"></script>
        <script defer="defer" src="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/de.js"></script>
        <script defer="defer" src="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/fr.js"></script>
        <script defer="defer" src="https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/es.js"></script>
        <script defer="defer" src="/assets/flatpickr-init.js"></script>
        <form class="filter-bar" method="GET" action="/">
            {(!prefix_for_hidden.is_empty()).then(|| view! {
                <input type="hidden" name="p" value=prefix_for_hidden.clone()/>
            })}
            <input
                type="text"
                name="q"
                class="filter-name"
                placeholder={t!("filter.name_placeholder")}
                value=q_init
                aria-label={t!("filter.name_label")}
            />
            // follow-up: pin `lang` on the date inputs so the
            // browser's native picker (calendar overlay, mm/dd order,
            // weekday names) follows the page locale instead of the
            // OS / browser language. `current_locale()` reads the live
            // `?lang=` signal — same source the `t!` macro uses.
            <label class="filter-date-label">
                {t!("filter.from")} ":"
                <input type="date" name="from" value=from_init lang={i18n::current_locale()}/>
            </label>
            <label class="filter-date-label">
                {t!("filter.to")} ":"
                <input type="date" name="to" value=to_init lang={i18n::current_locale()}/>
            </label>
            <button type="submit" class="tree-control-btn">{t!("filter.apply")}</button>
            {has_filter.then(|| view! {
                <a class="filter-clear" href=clear_filter_href(&prefix)>{t!("filter.clear")}</a>
            })}
        </form>
    }
}

/// Build the "clear filter" link — the same path the user is on but
/// without any of the `q` / `from` / `to` params. Used by the reset
/// link inside the filter bar.
fn clear_filter_href(prefix: &str) -> String {
    if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("/?p={}", url_encode(prefix))
    }
}

/// Full-catalog tree view. Two paths share this component:
///
/// - **Filter active** (`?q=*` / `?from=*` / `?to=*`): eager —
///   `get_catalog` returns every matching entry plus ancestor
///   directories so the surviving tree paths stay navigable. Same
///   behaviour as .
/// - **No filter**: lazy. Initial fetch is
///   `list_dir_page("", 0, PAGE_SIZE)`; folders render closed and load
///   their children only when the user opens the `<details>`. Each
///   level paginates with an IntersectionObserver-driven sentinel.
///
/// Sort key (`?sort=name|size|kind|date`) applies to both paths.
#[component]
fn CatalogTreeView(#[prop(into)] prefix: String) -> impl IntoView {
    use leptos_router::hooks::use_query_map;
    let query = use_query_map();
    let sort_signal = move || {
        query.with(|q| TreeSort::from_param(&q.get("sort").unwrap_or_default()))
    };
    let filter_signal = move || {
        query.with(|q| {
            (
                q.get("q").unwrap_or_default(),
                q.get("from").unwrap_or_default(),
                q.get("to").unwrap_or_default(),
            )
        })
    };
    let filter_active = move || {
        let (q, f, t) = filter_signal();
        !q.is_empty() || !f.is_empty() || !t.is_empty()
    };
    let filter_sig = Signal::derive(filter_signal);
    let sort_sig = Signal::derive(sort_signal);
    let prefix_breadcrumb = prefix.clone();
    let prefix_filter = prefix.clone();
    let prefix_lazy = prefix.clone();

    view! {
        // .x: when the tree root is not the catalog root,
        // show a breadcrumb so the user can navigate back up.
        {(!prefix_breadcrumb.is_empty()).then(|| view! {
            <Breadcrumb prefix=prefix_breadcrumb.clone()/>
        })}
        <p class="mut tree-intro">{t!("catalog.tree_intro")}</p>
        <FilterBar prefix=prefix_filter/>
        {move || {
            let p = prefix_lazy.clone();
            if filter_active() {
                // Eager / filter path stays prefix-agnostic for now —
                // filters are a "search the whole catalog" affordance
                // and rooting them at a subtree would surprise users.
                let _ = p;
                view! { <CatalogTreeEager
                    filter=filter_sig
                    sort=sort_sig
                /> }.into_any()
            } else {
                view! { <CatalogTreeLazy prefix=p sort=sort_sig/> }.into_any()
            }
        }}
    }
}

/// Eager tree: pulls the entire (filtered) catalog and builds the
/// tree client-side. Preserves behaviour for filter mode
/// — when the user types `q=*.png` we need to walk the whole catalog
/// to find matches, so paginated lazy loading buys us nothing.
#[component]
fn CatalogTreeEager(
    #[prop(into)] filter: Signal<(String, String, String)>,
    #[prop(into)] sort: Signal<TreeSort>,
) -> impl IntoView {
    let all = Resource::new(move || filter.get(), |(q, f, t)| async move {
        get_catalog(q, f, t).await
    });
    view! {
        <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
            {move || {
                let s = sort.get();
                all.get().map(|res| match res {
                    Ok(list) if list.is_empty() => {
                        // A5: eager tree only opens when a filter is
                        // active (see caller in `CatalogTreePage`), so
                        // an empty result here always means "the
                        // filter matched nothing", never "the catalog
                        // is empty". Give the user a way out.
                        let (q, f, t) = filter.get();
                        let filter_on = !q.is_empty() || !f.is_empty() || !t.is_empty();
                        if filter_on {
                            view! {
                                <p class="empty-state">
                                    {t!("catalog.no_results_for_filter")} " "
                                    <a href="/">{t!("catalog.clear_filter")}</a>
                                </p>
                            }.into_any()
                        } else {
                            view! {
                                <p class="empty-state">
                                    {t!("catalog.empty")} " " {t!("catalog.empty_hint")} " "
                                    <code>"curl -X PUT http://<host>/<name>"</code>
                                </p>
                            }.into_any()
                        }
                    },
                    Ok(list) => view! { <CatalogTreeBody entries=list sort=s/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })
            }}
        </Suspense>
    }
}

/// Page size for `list_dir_page`. Tuned so a typical folder renders
/// in one shot but a 10 000-entry root paginates instead of blocking.
const LAZY_PAGE_SIZE: u32 = 200;

/// Lazy tree. Only the root level is fetched
/// upfront via `list_dir_page("", 0, PAGE_SIZE)`; every directory
/// `<details>` triggers its own `list_dir_page(path, …)` the first
/// time it opens. The same controls bar (expand-all / sort / mkdir)
/// is shared with the eager path.
#[component]
fn CatalogTreeLazy(
    #[prop(into)] prefix: String,
    #[prop(into)] sort: Signal<TreeSort>,
) -> impl IntoView {
    // Reload the root level when the sort or prefix changes —
    // deeper opened folders re-mount their resource when their own
    // sort context changes, see `LazyDirNode`.
    let prefix_for_shell = prefix.clone();
    let root = Resource::new(
        {
            let p = prefix.clone();
            move || (p.clone(), sort.get().as_str())
        },
        |(p, s)| async move {
            list_dir_page(p, 0, LAZY_PAGE_SIZE, s.to_string()).await
        },
    );
    view! {
        <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
            {move || {
                let s = sort.get();
                let p = prefix_for_shell.clone();
                root.get().map(|res| match res {
                    Ok(page) if page.entries.is_empty() && !page.has_more => view! {
                        <p class="empty-state">
                            {t!("catalog.empty")} " " {t!("catalog.empty_hint")} " "
                            <code>"curl -X PUT http://<host>/<name>"</code>
                        </p>
                    }.into_any(),
                    Ok(page) => view! {
                        <CatalogTreeLazyShell prefix=p initial=page sort=s/>
                    }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })
            }}
        </Suspense>
    }
}

/// Lazy-tree shell: replicates the controls bar (expand / collapse /
/// mkdir / sort) the eager `CatalogTreeBody` renders, and mounts the
/// root `LazyLevel` for the actual entries.
#[component]
fn CatalogTreeLazyShell(
    #[prop(into)] prefix: String,
    initial: ListDirPage,
    sort: TreeSort,
) -> impl IntoView {
    let sort_link = |k: TreeSort| {
        if k == sort {
            "tree-sort-link active".to_string()
        } else {
            "tree-sort-link".to_string()
        }
    };
    let s_name = sort_link(TreeSort::Name);
    let s_size = sort_link(TreeSort::Size);
    let s_kind = sort_link(TreeSort::Kind);
    let s_date = sort_link(TreeSort::Date);
    // The root mkdir + upload forms create entries inside the
    // current tree root (catalog root when prefix is empty; the
    // chosen subfolder otherwise). `return_to` brings the user back
    // to the same root view so the new entry appears straight away.
    let prefix_for_mkdir = prefix.clone();
    let prefix_for_upload = prefix.clone();
    let return_to = if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("/?p={}", url_encode(&prefix))
    };
    let return_to_mkdir = return_to.clone();
    let return_to_upload = return_to.clone();
    let prefix_for_lazy = prefix.clone();
    view! {
        <div class="tree-controls">
            <button
                type="button"
                class="tree-control-btn tree-control-expand"
                onclick="holofsExpandAll(document.querySelector('.tree-root'))"
            >
                <span class="tree-control-icon">"⊕"</span>
                <span class="tree-control-label">{t!("tree.expand_all")}</span>
            </button>
            <button
                type="button"
                class="tree-control-btn tree-control-collapse"
                onclick="holofsCollapseAll(document.querySelector('.tree-root'))"
            >
                <span class="tree-control-icon">"⊖"</span>
                <span class="tree-control-label">{t!("tree.collapse_all")}</span>
            </button>
            <TreeZoomButtons/>
            <form class="tree-mkdir" method="POST" action="/api/mkdir">
                <input type="hidden" name="parent" value=prefix_for_mkdir/>
                <input type="hidden" name="return_to" value=return_to_mkdir/>
                <input
                    type="text"
                    name="name"
                    placeholder={t!("tree.new_folder")}
                    required=true
                    minlength="1"
                />
                <button type="submit" class="tree-control-btn">
                    <span class="tree-control-icon">"📁"</span>
                    <span class="tree-control-label">{t!("mkdir.submit")}</span>
                </button>
            </form>
            // Root upload form: drops a file straight into the
            // current tree root. When the catalog is at `/` the
            // parent is empty (top-level PUT); when the user has
            // cd'd into a subfolder via `?p=<path>`, the form is
            // scoped to that folder.
            <form
                class="tree-mkdir tree-root-upload"
                method="POST"
                action="/api/upload"
                enctype="multipart/form-data"
            >
                <input type="hidden" name="parent" value=prefix_for_upload/>
                <input type="hidden" name="return_to" value=return_to_upload/>
                <input type="file" name="file" required=true/>
                <button type="submit" class="tree-control-btn">
                    <span class="tree-control-icon">"↑"</span>
                    <span class="tree-control-label">{t!("upload.submit")}</span>
                </button>
            </form>
            <div class="tree-sort">
                <span class="tree-sort-label">{t!("tree.sort_by")} ":"</span>
                <a class=s_name href="/?sort=name">{t!("tree.sort.name")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_size href="/?sort=size">{t!("tree.sort.size")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_kind href="/?sort=kind">{t!("tree.sort.kind")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_date href="/?sort=date">{t!("tree.sort.date")}</a>
            </div>
        </div>
        <div class="tree-scroll">
            <ul class="tree-root">
                <LazyLevel
                    path=prefix_for_lazy
                    initial=initial
                    sort=sort
                    depth=0
                />
            </ul>
        </div>
        // sticky horizontal scrollbar proxy. The native
        // bar on `.tree-scroll` is hidden via CSS; this div is the
        // visible one, glued to the viewport bottom via `position:
        // sticky`. Inner spacer width is filled in by JS so the
        // proxy's scroll extent matches the tree's `scrollWidth`.
        <div class="tree-hscroll-proxy">
            <div class="tree-hscroll-proxy-inner"></div>
        </div>
        <script defer="defer" src="/assets/tree-hscroll.js"></script>
    }
}

/// One paginated, scrollable level inside the lazy tree.
///
/// Holds an `RwSignal<Vec<CatalogEntry>>` of accumulated rows plus a
/// `has_more` / `offset` pair. When `has_more` is true a sentinel
/// `<li>` is rendered at the bottom; an `IntersectionObserver`
/// installed on hydrate fires `load_more()` when the user scrolls it
/// into view. Without JS the sentinel acts as a click-to-load button.
#[component]
fn LazyLevel(
    path: String,
    initial: ListDirPage,
    sort: TreeSort,
    depth: usize,
) -> impl IntoView {
    let initial_len = initial.entries.len() as u32;
    let entries: RwSignal<Vec<CatalogEntry>> = RwSignal::new(initial.entries);
    let has_more: RwSignal<bool> = RwSignal::new(initial.has_more);
    let offset: RwSignal<u32> = RwSignal::new(initial_len);
    let is_loading: RwSignal<bool> = RwSignal::new(false);

    let path_for_load = path.clone();
    let sort_str = sort.as_str().to_string();
    let load_more = move || {
        if !has_more.get_untracked() || is_loading.get_untracked() {
            return;
        }
        is_loading.set(true);
        let p = path_for_load.clone();
        let off = offset.get_untracked();
        let srt = sort_str.clone();
        leptos::task::spawn_local(async move {
            match list_dir_page(p, off, LAZY_PAGE_SIZE, srt).await {
                Ok(page) => {
                    let n = page.entries.len() as u32;
                    entries.update(|v| v.extend(page.entries));
                    offset.set(off + n);
                    has_more.set(page.has_more);
                }
                Err(_) => { /* leave state intact; user can click sentinel to retry */ }
            }
            is_loading.set(false);
        });
    };
    let load_more_for_click = load_more.clone();
    let on_sentinel_click = move |_| load_more_for_click();

    let sentinel_ref: NodeRef<leptos::html::Li> = NodeRef::new();
    let _ = sentinel_ref;
    let _ = load_more;
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            use wasm_bindgen::{closure::Closure, JsCast};
            let Some(el) = sentinel_ref.get() else { return; };
            // Once the sentinel scrolls into view (any pixel
            // visible), fire load_more. The observer keeps watching
            // — when the next batch arrives the sentinel may still
            // be in view (e.g. user scrolled past it) and we'll
            // fetch again.
            let load = load_more.clone();
            let cb = Closure::<dyn FnMut(js_sys::Array)>::new(
                move |entries: js_sys::Array| {
                    for i in 0..entries.length() {
                        let Ok(entry): Result<web_sys::IntersectionObserverEntry, _> =
                            entries.get(i).dyn_into() else { continue };
                        if entry.is_intersecting() {
                            load();
                            break;
                        }
                    }
                },
            );
            if let Ok(obs) = web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) {
                let el_ref: &web_sys::Element = el.as_ref();
                obs.observe(el_ref);
                cb.forget();
                std::mem::forget(obs);
            }
        });
    }

    // Sort moves into the For body. RwSignal is Copy; sort + depth
    // also Copy, so the children closure can read them freely.
    view! {
        <For
            each=move || entries.get()
            key=|e: &CatalogEntry| e.name.clone()
            children=move |e: CatalogEntry| {
                if e.kind == "directory" {
                    view! {
                        <LazyDirNode entry=e sort=sort depth=depth/>
                    }.into_any()
                } else {
                    lazy_file_leaf(&e).into_any()
                }
            }
        />
        {move || {
            if has_more.get() {
                view! {
                    <li
                        node_ref=sentinel_ref
                        class="tree-sentinel mut"
                        on:click=on_sentinel_click.clone()
                        title="load more"
                    >
                        {move || if is_loading.get() {
                            t!("catalog.loading")
                        } else {
                            t!("tree.load_more")
                        }}
                    </li>
                }.into_any()
            } else {
                ().into_any()
            }
        }}
    }
}

/// One file row inside the lazy tree. Standalone helper so the markup
/// stays in lock-step with the eager `TreeNodeView` leaf branch.
#[cfg(feature = "ssr")]
fn lazy_file_leaf(entry: &CatalogEntry) -> impl IntoView {
    lazy_file_leaf_inner(entry.clone())
}
#[cfg(not(feature = "ssr"))]
fn lazy_file_leaf(entry: &CatalogEntry) -> impl IntoView {
    lazy_file_leaf_inner(entry.clone())
}
fn lazy_file_leaf_inner(entry: CatalogEntry) -> impl IntoView {
    let enc_full = url_encode(&entry.name);
    let kind = entry.kind.clone();
    let basename = entry
        .name
        .rsplit_once('/')
        .map(|(_, b)| b.to_string())
        .unwrap_or_else(|| entry.name.clone());
    let size_str = format_bytes(entry.bytes_stored);
    let date_str = format_unix_utc(entry.created_at_unix);
    let icon: &'static str = match kind.as_str() {
        "image" => "🖼",
        "audio" => "🎵",
        "text" => "📝",
        _ => "📦",
    };
    let file_confirm_js = format!(
        "return confirm('{}');",
        t!("file.delete_confirm").replace('\'', "\\'")
    );
    view! {
        <li class={format!("tree-leaf kind-{kind}")}>
            <span class="tree-icon">{icon}</span>
            <a class="tree-name" href={format!("/{enc_full}")} rel="external">{basename}</a>
            <span class="tree-meta">
                <span class="tree-meta-size" title="shard count">{size_str}</span>
                <span class="tree-meta-date" title="created (UTC)">{date_str}</span>
            </span>
            <span class="tree-sep">"·"</span>
            <span class="tree-actions">
                {(kind == "image" || kind == "audio").then(|| view! {
                    <a href={format!("/preview/{}", enc_full.clone())} rel="external">"preview"</a>
                    <span class="tree-sep">"·"</span>
                })}
                <a href={format!("/inspect/{}", enc_full.clone())} rel="external">"shards"</a>
                <span class="tree-sep">"·"</span>
                <a href={format!("/similar/{}", enc_full.clone())} rel="external">{t!("card.action.similar")}</a>
                <span class="tree-sep">"·"</span>
                <a href={format!("/health/{}", enc_full.clone())} rel="external">{t!("card.action.health")}</a>
                {(kind == "image").then(|| view! {
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/mix?a={}", enc_full.clone())} rel="external">{t!("mix.link_label")}</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/holo/{}", enc_full.clone())} rel="external">{t!("holo.link_label")}</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/spotlight?a={}", enc_full.clone())} rel="external">{t!("spotlight.link_label")}</a>
                })}
                <span class="tree-sep">"·"</span>
                <a href={format!("/versions/{}", enc_full.clone())} rel="external">{t!("versions.link_label")}</a>
                <span class="tree-sep">"·"</span>
                <form
                    method="POST"
                    action="/api/rm"
                    class="inline-form"
                    onsubmit=file_confirm_js
                >
                    <input type="hidden" name="path" value=entry.name.clone()/>
                    <input type="hidden" name="return_to" value="/"/>
                    <button
                        type="submit"
                        class="link-btn link-btn-danger"
                        title={t!("file.delete")}
                    >
                        "✕"
                    </button>
                </form>
            </span>
        </li>
    }
}

/// One folder row inside the lazy tree — `<details>` whose children
/// only fetch on the first `open`. Folder summary contents (icon /
/// name / mkdir / open / delete actions) mirror the eager branch.
#[component]
fn LazyDirNode(entry: CatalogEntry, sort: TreeSort, depth: usize) -> impl IntoView {
    let path = entry.name.clone();
    let basename = path
        .rsplit_once('/')
        .map(|(_, b)| b.to_string())
        .unwrap_or_else(|| path.clone());
    let enc_path = url_encode(&path);
    let created_str = format_unix_utc(entry.created_at_unix);
    let confirm_js = format!(
        "return confirm('{}');",
        t!("folder.confirm_delete").replace('\'', "\\'")
    );

    // Resource-based lazy loading turned out to be
    // brittle because the SSR cached an "empty" value for closed
    // folders (open_sig=false → loader short-circuits) and Leptos
    // didn't reliably re-run the loader on hydrate when the source
    // flipped. We now drive the fetch explicitly: an Action shipped
    // a `list_dir_page` per `open` toggle, the result lands in a
    // plain `RwSignal<Option<…>>`, and the view reads that.
    //
    // For depth==0 (top-level folders that should appear expanded on
    // first paint), we kick off an immediate fetch on mount so SSR
    // streams a populated subtree. Deeper levels stay quiet until
    // the user clicks.
    //
    // also auto-open any folder whose path is an
    // ancestor of (or equal to) the `?open=<path>` query param. This
    // is how the mkdir form returns the user to the tree with the
    // freshly-created folder visible — return_to=`/?open=<parent>`
    // unrolls the chain of `<details>` down to that point.
    let open_target = use_query_map().with_untracked(|q| {
        // Read `?open=<path>` first (the modern auto-expand
        // mechanism). Fall back to the legacy `?p=<path>` query
        // param so bookmarks from the deleted focus-view page still
        // expand the right folder.
        q.get("open")
            .or_else(|| q.get("p"))
            .unwrap_or_default()
    });
    let auto_open = !open_target.is_empty()
        && (open_target == path || open_target.starts_with(&format!("{path}/")));
    let initial_open = depth == 0 || auto_open;
    // the target node (exact match with `?open=`) is
    // where the user just acted (mkdir), so we scroll it into view
    // once it hydrates. Ancestor chain auto-opens but doesn't grab
    // scroll focus — that would yank the page upward in the middle
    // of loading deeper levels.
    let is_open_target = !open_target.is_empty() && open_target == path;
    let details_ref: NodeRef<leptos::html::Details> = NodeRef::new();
    let open_sig = RwSignal::new(initial_open);
    // Replace the previous spawn_local + RwSignal pattern with a
    // keyed `Resource`. Reason: under streaming-hydrate the
    // `Effect::new(...)` that used to kick off `spawn_local` never
    // ran its initial pass for components mounted late in the
    // hydration stream — so depth-0 `initial_open=true` folders
    // sat under a stuck "loading catalog…" placeholder until the
    // user clicked. Resource is what the root catalog already
    // uses (see `CatalogTreeLazy`), and its hydrate behaviour is
    // well-trodden: it picks up the SSR-resolved payload if any,
    // or fetches eagerly post-mount.
    //
    // The key is `(open, path, sort)`; while `open=false` we hand
    // back a sentinel `None`-ish value so no fetch fires until the
    // user opens the folder.
    let path_for_load = path.clone();
    let sort_str = sort.as_str().to_string();
    // Key the resource on (path, sort) only. Every folder fetches
    // its direct children eagerly during SSR so the initial HTML
    // arrives populated end-to-end — no hydrate-side refetch
    // required, which matters because `Effect::new` for
    // late-mounted lazy components doesn't reliably fire its
    // initial pass under streaming hydration (proven empirically:
    // both the old `spawn_local` path and a `(open, …)`-keyed
    // Resource left every depth-0 row stuck at "loading catalog…").
    // For the dev cluster's 17 directories this is one paginated
    // RPC per folder during SSR; the user perceives a single
    // initial page render. The disclosure triangle then just toggles
    // the native `<details>` visibility — data is already there.
    let children_res = Resource::new(
        move || (path_for_load.clone(), sort_str.clone()),
        |(p, srt)| async move { list_dir_page(p, 0, LAZY_PAGE_SIZE, srt).await },
    );

    // scroll the open-target folder into view once it
    // mounts on the client. Without this the redirect after mkdir
    // dumps the user at the top of the page even though the right
    // `<details>` is already expanded somewhere below.
    #[cfg(feature = "hydrate")]
    if is_open_target {
        Effect::new(move |_| {
            if let Some(el) = details_ref.get() {
                let el: web_sys::Element = (*el).clone().into();
                el.scroll_into_view();
            }
        });
    }

    // Authoritative signal → DOM sync. We can't bind `open=move ||
    // open_sig.get()` directly on the `<details>` because leptos
    // 0.7's attribute writer sometimes renders `open="false"`
    // (attribute present → browser keeps the disclosure open) when
    // the closure returns false. `set_open(bool)` on the typed
    // HtmlDetailsElement bypasses that quirk and toggles the
    // attribute presence correctly. Runs on every signal flip after
    // hydrate.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let want = open_sig.get();
        if let Some(el) = details_ref.get() {
            use wasm_bindgen::JsCast;
            let el: web_sys::Element = (*el).clone().into();
            if let Ok(d) = el.dyn_into::<web_sys::HtmlDetailsElement>() {
                if d.open() != want {
                    d.set_open(want);
                }
            }
        }
    });

    let on_toggle = move |ev: leptos::ev::Event| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::JsCast;
            let elem = ev
                .current_target()
                .or_else(|| ev.target())
                .and_then(|t| t.dyn_into::<web_sys::HtmlDetailsElement>().ok());
            if let Some(d) = elem {
                open_sig.set(d.open());
            }
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = ev;
    };

    let path_for_form = path.clone();
    let path_for_rmdir = path.clone();
    let path_for_upload = path.clone();
    let inner_path = path.clone();
    view! {
        <li class="tree-branch">
            // SSR-time `open` attribute mirrors the initial signal
            // value: top-level folders ship open in the HTML,
            // deeper ones closed. After hydrate the Effect above
            // becomes the single source of truth and re-applies
            // `set_open(open_sig.get())` whenever the signal flips
            // — that's what makes JS-driven `holofsExpandAll` /
            // `holofsCollapseAll` survive the surrounding
            // `<ul class="tree-children">` re-renders that follow
            // a lazy-fetch resolution.
            <details node_ref=details_ref open=initial_open on:toggle=on_toggle>
                <summary class="tree-summary">
                    <span class="tree-icon">"📁"</span>
                    <span class="tree-name">{basename}</span>
                    <span class="tree-meta">
                        <span class="tree-meta-date" title="created (UTC)">{created_str}</span>
                    </span>
                    <span class="tree-sep">"·"</span>
                    <span class="tree-actions">
                        // per-folder ⊕ / ⊖ buttons.
                        // `holofsExpandAll(closest details)` opens
                        // this folder and every descendant — the
                        // helper retries until lazy fetches settle.
                        // `stopPropagation` keeps the click from
                        // toggling the `<details>` itself.
                        <button
                            type="button"
                            class="link-btn tree-action-zoom"
                            title={t!("tree.expand_subtree")}
                            onclick="event.stopPropagation();holofsExpandAll(this.closest('details'))"
                        >
                            "⊕"
                        </button>
                        <button
                            type="button"
                            class="link-btn tree-action-zoom"
                            title={t!("tree.collapse_subtree")}
                            onclick="event.stopPropagation();holofsCollapseAll(this.closest('details'))"
                        >
                            "⊖"
                        </button>
                        <span class="tree-sep">"·"</span>
                        <form
                            method="POST"
                            action="/api/mkdir"
                            class="inline-form tree-inline-mkdir"
                            onclick="event.stopPropagation()"
                        >
                            <input type="hidden" name="parent" value=path_for_form/>
                            // land back on the tree view
                            // with this folder expanded so the new
                            // child is visible — `?open=<path>`
                            // unrolls the `<details>` chain down to
                            // it. Beats throwing the user into the
                            // focus view, which loses the tree state.
                            <input
                                type="hidden"
                                name="return_to"
                                value={format!("/?p={enc_path}")}
                            />
                            <input
                                type="text"
                                name="name"
                                placeholder={t!("tree.new_folder")}
                                required=true
                                minlength="1"
                            />
                            <button type="submit" class="link-btn">"+ " {t!("folder.kind_label")}</button>
                        </form>
                        <span class="tree-sep">"·"</span>
                        // Inline upload: drop a file straight into this
                        // folder, no detour through the topbar Upload
                        // page. Mirrors the per-folder mkdir form; same
                        // ?open=<path> return so the tree re-opens at
                        // exactly this branch after the multipart POST.
                        <form
                            method="POST"
                            action="/api/upload"
                            enctype="multipart/form-data"
                            class="inline-form tree-inline-upload"
                            onclick="event.stopPropagation()"
                        >
                            <input type="hidden" name="parent" value=path_for_upload/>
                            <input
                                type="hidden"
                                name="return_to"
                                value={format!("/?p={enc_path}")}
                            />
                            <input type="file" name="file" required=true/>
                            <button type="submit" class="link-btn">"↑ " {t!("upload.submit")}</button>
                        </form>
                        <span class="tree-sep">"·"</span>
                        <a href={format!("/?p={enc_path}")} rel="external">{t!("folder.open")} " →"</a>
                        <span class="tree-sep">"·"</span>
                        <form
                            method="POST"
                            action="/api/rmdir"
                            class="inline-form"
                            onsubmit=confirm_js
                        >
                            <input type="hidden" name="path" value=path_for_rmdir/>
                            <input type="hidden" name="return_to" value="/"/>
                            <button type="submit" class="link-btn">{t!("folder.delete")}</button>
                        </form>
                    </span>
                </summary>
                <ul class="tree-children">
                    // render the resource's value directly
                    // instead of inside `<Suspense>`. The Suspense
                    // streamed an empty template on SSR (because
                    // `open_sig=false` short-circuits the loader); on
                    // hydrate it treated the resource as "resolved
                    // forever" and never re-rendered when the user
                    // opened the folder — even though the Resource's
                    // source changed and a fresh fetch was kicked off,
                    // the Suspense fallback child closure stayed stale.
                    // Reading `.get()` directly here re-runs on every
                    // resource state transition (pending / resolved).
                    {move || {
                        let p = inner_path.clone();
                        let val = children_res.get();
                        let is_open = open_sig.get();
                        match val {
                            // Resource never resolved yet AND folder
                            // never opened — render nothing.
                            None if !is_open => ().into_any(),
                            // Open but the resource is still pending.
                            None => view! {
                                <li class="mut">{t!("catalog.loading")}</li>
                            }.into_any(),
                            Some(Ok(page)) if page.entries.is_empty() && !page.has_more => {
                                ().into_any()
                            }
                            Some(Ok(page)) => view! {
                                <LazyLevel
                                    path=p
                                    initial=page
                                    sort=sort
                                    depth=depth + 1
                                />
                            }.into_any(),
                            Some(Err(e)) => view! {
                                <li class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</li>
                            }.into_any(),
                        }
                    }}
                </ul>
            </details>
        </li>
    }
}

/// Renders the body of the tree (controls + scrollable list). The
/// expand/collapse-all buttons run a one-line JS handler that walks
/// every `<details>` inside `.tree-root` and toggles the `open`
/// attribute. Without JS the tree still works — every top-level node
/// starts open, deeper ones closed.
#[component]
fn CatalogTreeBody(entries: Vec<CatalogEntry>, sort: TreeSort) -> impl IntoView {
    let tree = build_tree(entries, sort);
    let sort_link = |k: TreeSort| {
        if k == sort {
            format!("tree-sort-link active")
        } else {
            "tree-sort-link".to_string()
        }
    };
    let s_name = sort_link(TreeSort::Name);
    let s_size = sort_link(TreeSort::Size);
    let s_kind = sort_link(TreeSort::Kind);
    let s_date = sort_link(TreeSort::Date);
    view! {
        <div class="tree-controls">
            <button
                type="button"
                class="tree-control-btn tree-control-expand"
                onclick="holofsExpandAll(document.querySelector('.tree-root'))"
            >
                <span class="tree-control-icon">"⊕"</span>
                <span class="tree-control-label">{t!("tree.expand_all")}</span>
            </button>
            <button
                type="button"
                class="tree-control-btn tree-control-collapse"
                onclick="holofsCollapseAll(document.querySelector('.tree-root'))"
            >
                <span class="tree-control-icon">"⊖"</span>
                <span class="tree-control-label">{t!("tree.collapse_all")}</span>
            </button>
            <TreeZoomButtons/>
            <form class="tree-mkdir" method="POST" action="/api/mkdir">
                <input type="hidden" name="parent" value=""/>
                <input type="hidden" name="return_to" value="/"/>
                <input
                    type="text"
                    name="name"
                    placeholder={t!("tree.new_folder")}
                    required=true
                    minlength="1"
                />
                <button type="submit" class="tree-control-btn">
                    <span class="tree-control-icon">"📁"</span>
                    <span class="tree-control-label">{t!("mkdir.submit")}</span>
                </button>
            </form>
            // Root upload form: drop a file at the catalog root with
            // no folder prefix. Mirrors the per-folder inline upload
            // a level down; the empty `parent` value is what
            // `handlers::upload_form` expects for a top-level PUT.
            <form
                class="tree-mkdir tree-root-upload"
                method="POST"
                action="/api/upload"
                enctype="multipart/form-data"
            >
                <input type="hidden" name="parent" value=""/>
                <input type="hidden" name="return_to" value="/"/>
                <input type="file" name="file" required=true/>
                <button type="submit" class="tree-control-btn">
                    <span class="tree-control-icon">"↑"</span>
                    <span class="tree-control-label">{t!("upload.submit")}</span>
                </button>
            </form>
            <div class="tree-sort">
                <span class="tree-sort-label">{t!("tree.sort_by")} ":"</span>
                <a class=s_name href="/?sort=name">{t!("tree.sort.name")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_size href="/?sort=size">{t!("tree.sort.size")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_kind href="/?sort=kind">{t!("tree.sort.kind")}</a>
                <span class="tree-sep">"·"</span>
                <a class=s_date href="/?sort=date">{t!("tree.sort.date")}</a>
            </div>
        </div>
        <div class="tree-scroll">
            <ul class="tree-root">
                {tree.children.into_iter().map(|child| {
                    view! { <TreeNodeView node=child depth=0/> }
                }).collect_view()}
            </ul>
        </div>
        // sticky H-scroll proxy. See the LazyShell
        // version for background; the init script lives in `Shell`
        // (top-level <head>) because `<script>` tags injected via
        // streamed Suspense templates don't execute per HTML5 spec.
        <div class="tree-hscroll-proxy">
            <div class="tree-hscroll-proxy-inner"></div>
        </div>
    }
}

/// One in-memory tree node. Either a directory (with children) or an
/// object leaf (with the original `CatalogEntry` for metadata).
#[derive(Clone, Debug)]
struct TreeNode {
    /// Full catalog path (`photos/2026/img.jpg`). Empty for the synthetic root.
    path: String,
    /// Last path segment, displayed in the tree row.
    basename: String,
    /// `Some` for objects; `None` for directories.
    entry: Option<CatalogEntry>,
    /// Sorted child nodes — directories first, then objects, both
    /// alphabetical inside each bucket.
    children: Vec<TreeNode>,
}

/// Build a tree out of a flat list of `CatalogEntry`. Directories are
/// implied by entries with `kind == "directory"`; objects attach to the
/// directory matching their parent path (or the root). Missing parent
/// markers are tolerated — synthesized directory placeholders are
/// inserted when an object's parent path isn't already in the catalog.
///
/// `sort` controls the within-directory ordering. Directories always
/// sort to the top of their parent; objects fall to the bottom and are
/// ranked by the selected key.
fn build_tree(entries: Vec<CatalogEntry>, sort: TreeSort) -> TreeNode {
    use std::collections::BTreeMap;

    // Pass 1: collect every full path we'll need a node for. Object
    // entries register themselves and every ancestor along the way
    // (defensively synthesising directories the catalog may have
    // skipped).
    let mut all_paths: BTreeMap<String, Option<CatalogEntry>> = BTreeMap::new();
    for e in entries {
        // Walk ancestors; insert placeholders only if not already known.
        let mut acc = String::new();
        for seg in e.name.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            all_paths.entry(acc.clone()).or_insert(None);
        }
        // Now overwrite the final path with the actual entry.
        all_paths.insert(e.name.clone(), Some(e));
    }

    // Pass 2: group every path by its parent. The empty string keys
    // hold the top-level entries; for nested paths the key is the
    // substring before the last `/`.
    let mut children_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in all_paths.keys() {
        let parent = match path.rsplit_once('/') {
            Some((p, _)) => p.to_string(),
            None => String::new(),
        };
        children_by_parent
            .entry(parent)
            .or_default()
            .push(path.clone());
    }

    // Pass 3: recursively assemble nodes top-down. Returns the
    // synthetic root with all top-level entries as children. `sort` is
    // threaded through so the recursion uses the same key at every
    // depth.
    fn build(
        path: &str,
        all: &BTreeMap<String, Option<CatalogEntry>>,
        by_parent: &BTreeMap<String, Vec<String>>,
        sort: TreeSort,
    ) -> TreeNode {
        let basename = match path.rsplit_once('/') {
            Some((_, l)) => l.to_string(),
            None => path.to_string(),
        };
        let entry = all.get(path).and_then(|v| v.clone());
        let mut children: Vec<TreeNode> = by_parent
            .get(path)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|cpath| build(cpath, all, by_parent, sort))
            .collect();
        // Directories first, then objects; the chosen key drives
        // ordering inside each bucket. A node is a "directory" iff its
        // entry is missing (synthesized) or marked as such.
        let is_dir = |n: &TreeNode| {
            n.entry.as_ref().map_or(true, |e| e.kind == "directory")
        };
        children.sort_by(|a, b| {
            let a_dir = is_dir(a);
            let b_dir = is_dir(b);
            if a_dir != b_dir {
                return b_dir.cmp(&a_dir);
            }
            // Both same bucket — apply the user-chosen ordering.
            match sort {
                TreeSort::Name => a.basename.cmp(&b.basename),
                TreeSort::Size => {
                    // Directories report 0 → tied, fall back to name.
                    let a_n = a.entry.as_ref().map_or(0, |e| e.n_shards);
                    let b_n = b.entry.as_ref().map_or(0, |e| e.n_shards);
                    b_n.cmp(&a_n).then_with(|| a.basename.cmp(&b.basename))
                }
                TreeSort::Kind => {
                    let ka = a.entry.as_ref().map(|e| e.kind.as_str()).unwrap_or("directory");
                    let kb = b.entry.as_ref().map(|e| e.kind.as_str()).unwrap_or("directory");
                    ka.cmp(kb).then_with(|| a.basename.cmp(&b.basename))
                }
                TreeSort::Date => {
                    // Newest first. `created_at_unix == 0` means
                    // "unknown" (legacy manifest before ) and
                    // we shove those to the bottom of the bucket so the
                    // populated timestamps surface up top.
                    let a_t = a.entry.as_ref().map_or(0u64, |e| e.created_at_unix);
                    let b_t = b.entry.as_ref().map_or(0u64, |e| e.created_at_unix);
                    let a_known = a_t > 0;
                    let b_known = b_t > 0;
                    match (a_known, b_known) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => b_t.cmp(&a_t).then_with(|| a.basename.cmp(&b.basename)),
                    }
                }
            }
        });
        TreeNode {
            path: path.to_string(),
            basename,
            entry,
            children,
        }
    }
    build("", &all_paths, &children_by_parent, sort)
}

/// Recursive view for a tree node. Directories render as `<details>`
/// (top-level dirs default-open, deeper ones default-closed so the
/// initial paint isn't an explosion). Objects render as compact `<li>`
/// rows with the same set of actions as the focus-view's `ObjectCard`.
#[component]
fn TreeNodeView(node: TreeNode, depth: usize) -> impl IntoView {
    // A node is a directory branch when it has no `CatalogEntry` (a
    // synthesized intermediary) OR when the entry itself is a
    // `Directory` marker. Everything else is an object leaf.
    let is_directory = node
        .entry
        .as_ref()
        .map_or(true, |e| e.kind == "directory");
    if !is_directory {
        let entry = node.entry.clone().expect("non-dir → entry is Some");
        // Object leaf.
        let enc_full = url_encode(&entry.name);
        let kind = entry.kind.clone();
        let basename = node.basename.clone();
        let size_str = format_bytes(entry.bytes_stored);
        let date_str = format_unix_utc(entry.created_at_unix);
        let icon: &'static str = match kind.as_str() {
            "image" => "🖼",
            "audio" => "🎵",
            "text" => "📝",
            _ => "📦",
        };
        // confirm dialog before the form POSTs. The
        // translated string is interpolated raw so we double single
        // quotes to keep it inside the JS string literal.
        let file_confirm_js = format!(
            "return confirm('{}');",
            t!("file.delete_confirm").replace('\'', "\\'")
        );
        view! {
            <li class={format!("tree-leaf kind-{kind}")}>
                <span class="tree-icon">{icon}</span>
                <a class="tree-name" href={format!("/{enc_full}")} rel="external">{basename}</a>
                <span class="tree-meta">
                    <span class="tree-meta-size" title="shard count">{size_str}</span>
                    <span class="tree-meta-date" title="created (UTC)">{date_str}</span>
                </span>
                <span class="tree-sep">"·"</span>
                <span class="tree-actions">
                    {(kind == "image" || kind == "audio").then(|| view! {
                        <a href={format!("/preview/{}", enc_full.clone())} rel="external">"preview"</a>
                        <span class="tree-sep">"·"</span>
                    })}
                    <a href={format!("/inspect/{}", enc_full.clone())} rel="external">"shards"</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/similar/{}", enc_full.clone())} rel="external">{t!("card.action.similar")}</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/health/{}", enc_full.clone())} rel="external">{t!("card.action.health")}</a>
                    {(kind == "image").then(|| view! {
                        <span class="tree-sep">"·"</span>
                        <a href={format!("/mix?a={}", enc_full.clone())} rel="external">{t!("mix.link_label")}</a>
                    })}
                    <span class="tree-sep">"·"</span>
                    <form
                        method="POST"
                        action="/api/rm"
                        class="inline-form"
                        onsubmit=file_confirm_js
                    >
                        <input type="hidden" name="path" value=entry.name.clone()/>
                        <input type="hidden" name="return_to" value="/"/>
                        <button
                            type="submit"
                            class="link-btn link-btn-danger"
                            title={t!("file.delete")}
                        >
                            "✕"
                        </button>
                    </form>
                </span>
            </li>
        }
        .into_any()
    } else {
        // Directory branch. Top-level (depth 0) defaults to open;
        // deeper ones start closed.
        let path = node.path.clone();
        let enc_path = url_encode(&path);
        let n_children = node.children.len();
        let confirm_js = format!(
            "return confirm('{}');",
            t!("folder.confirm_delete").replace('\'', "\\'")
        );
        let children = node.children.clone();
        let open_attr = depth == 0;
        let created_str = node
            .entry
            .as_ref()
            .map(|e| format_unix_utc(e.created_at_unix))
            .unwrap_or_else(|| "—".to_string());
        view! {
            <li class="tree-branch">
                <details open=open_attr>
                    <summary class="tree-summary">
                        <span class="tree-icon">"📁"</span>
                        <span class="tree-name">{node.basename.clone()}</span>
                        <span class="tree-count">"(" {n_children} ")"</span>
                        <span class="tree-meta">
                            <span class="tree-meta-date" title="created (UTC)">{created_str}</span>
                        </span>
                        <span class="tree-sep">"·"</span>
                        <span class="tree-actions">
                            // same per-folder ⊕ / ⊖ as
                            // the lazy view. Eager mode just lets the
                            // helper walk an in-DOM tree — no fetch
                            // retries actually fire because all
                            // descendants are already rendered.
                            <button
                                type="button"
                                class="link-btn tree-action-zoom"
                                title={t!("tree.expand_subtree")}
                                onclick="event.stopPropagation();holofsExpandAll(this.closest('details'))"
                            >
                                "⊕"
                            </button>
                            <button
                                type="button"
                                class="link-btn tree-action-zoom"
                                title={t!("tree.collapse_subtree")}
                                onclick="event.stopPropagation();holofsCollapseAll(this.closest('details'))"
                            >
                                "⊖"
                            </button>
                            <span class="tree-sep">"·"</span>
                            <form
                                method="POST"
                                action="/api/mkdir"
                                class="inline-form tree-inline-mkdir"
                                onclick="event.stopPropagation()"
                            >
                                <input type="hidden" name="parent" value=path.clone()/>
                                // same `?open=` trick the
                                // lazy tree uses — keeps the user on
                                // the tree view with this folder
                                // pre-expanded.
                                <input
                                    type="hidden"
                                    name="return_to"
                                    value={format!("/?p={enc_path}")}
                                />
                                <input
                                    type="text"
                                    name="name"
                                    placeholder={t!("tree.new_folder")}
                                    required=true
                                    minlength="1"
                                />
                                <button type="submit" class="link-btn">"+ " {t!("folder.kind_label")}</button>
                            </form>
                            <span class="tree-sep">"·"</span>
                            <form
                                method="POST"
                                action="/api/upload"
                                enctype="multipart/form-data"
                                class="inline-form tree-inline-upload"
                                onclick="event.stopPropagation()"
                            >
                                <input type="hidden" name="parent" value=path.clone()/>
                                <input
                                    type="hidden"
                                    name="return_to"
                                    value={format!("/?p={enc_path}")}
                                />
                                <input type="file" name="file" required=true/>
                                <button type="submit" class="link-btn">"↑ " {t!("upload.submit")}</button>
                            </form>
                            <span class="tree-sep">"·"</span>
                            <a href={format!("/?p={enc_path}")} rel="external">{t!("folder.open")} " →"</a>
                            <span class="tree-sep">"·"</span>
                            <form
                                method="POST"
                                action="/api/rmdir"
                                class="inline-form"
                                onsubmit=confirm_js
                            >
                                <input type="hidden" name="path" value=path.clone()/>
                                <input type="hidden" name="return_to" value="/"/>
                                <button type="submit" class="link-btn">{t!("folder.delete")}</button>
                            </form>
                        </span>
                    </summary>
                    <ul class="tree-children">
                        {children.into_iter().map(|c| view! {
                            <TreeNodeView node=c depth={depth + 1}/>
                        }).collect_view()}
                    </ul>
                </details>
            </li>
        }
        .into_any()
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
            <a href="/" rel="external">{t!("breadcrumb.home")}</a>
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
                        <a href={format!("/?p={enc}")} rel="external">{seg}</a>
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
/// polish: the native `<input type="file">` is visually-hidden;
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
/// gateway's HTML: thumb on top, metadata table, actions row. /// added the `parent` prop so the basename ("img.jpg" out of
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
        n_shards: _,
        bytes_stored,
        cid_short,
        audio_sample_rate,
        channels,
        created_at_unix: _,
    } = entry;
    let size_str = format_bytes(bytes_stored);

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
                <a class="thumb dir-thumb" href={format!("/?p={enc_full}")} rel="external">
                    <span class="icon">"📁"</span>
                </a>
                <div class="meta">
                    <div class="name">
                        <a href={format!("/?p={enc_full}")} rel="external">{basename.clone()}</a>
                    </div>
                    <div class="row mut">{t!("folder.kind_label")}</div>
                </div>
                <div class="actions">
                    <a href={format!("/?p={}", enc_full.clone())} rel="external">{t!("folder.open")}</a>
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
    let file_confirm_js = format!(
        "return confirm('{}');",
        t!("file.delete_confirm").replace('\'', "\\'")
    );
    let return_to = if parent.is_empty() {
        "/".to_string()
    } else {
        format!("/?p={}", url_encode(&parent))
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
                <div class="row mut">{size_str}</div>
                <div class="row mut cid"><code>{cid_short}</code></div>
            </div>
            <div class="actions">
                // A6 · UI-UX-review: was a flat "·"-separated list
                // of up to 10 links with delete at the end, one
                // class away from `holo`/`spotlight`. High misclick
                // risk on the destructive action. Now: two primary
                // links inline (open + preview), a `<details>` "⋯"
                // popover for the six-to-seven secondary actions,
                // then delete after a visual separator.
                //
                // Native `<details>` toggle is progressive: no-JS
                // clients still see and use the menu; the hydrate-
                // only click-outside handler in mutation-forms.js
                // upgrades to popover-style dismissal.
                <a href={format!("/{enc_full}")} rel="external">{primary_label(&kind)}</a>
                {(kind == "image" || kind == "audio").then(|| view! {
                    " · " <a href={format!("/preview/{}", enc_full.clone())} rel="external">{preview_label(&kind)}</a>
                })}
                " · "
                <details class="card-more js-dropdown">
                    <summary aria-label={t!("card.more_actions")} title={t!("card.more_actions")}>"⋯"</summary>
                    <div class="card-more-menu" role="menu">
                        <a href={format!("/inspect/{}", enc_full.clone())} rel="external">{t!("card.action.shards_link")}</a>
                        <a href={format!("/similar/{}", enc_full.clone())} rel="external">{t!("card.action.similar")}</a>
                        <a href={format!("/health/{}", enc_full.clone())} rel="external">{t!("card.action.health")}</a>
                        {(kind == "image").then(|| view! {
                            <a href={format!("/mix?a={}", enc_full.clone())} rel="external">{t!("mix.link_label")}</a>
                            <a href={format!("/holo/{}", enc_full.clone())} rel="external">{t!("holo.link_label")}</a>
                            <a href={format!("/spotlight?a={}", enc_full.clone())} rel="external">{t!("spotlight.link_label")}</a>
                        })}
                        <a href={format!("/versions/{}", enc_full.clone())} rel="external">{t!("versions.link_label")}</a>
                    </div>
                </details>
                // A10 · UI-UX: inline rename form. `POST /api/mv`
                // already handled server-side, no UI existed — so
                // users couldn't rename via the interface. Sits
                // between the "⋯" popover and the delete form; the
                // mutation-forms.js interceptor catches the submit
                // and toasts / reloads on success. Placeholder-only
                // client-side validation for now; server rejects
                // reserved names.
                <details class="card-rename js-dropdown">
                    <summary title={t!("card.rename")}>{t!("card.rename")}</summary>
                    <form
                        method="POST"
                        action="/api/mv"
                        class="card-rename-form inline-form"
                    >
                        <input type="hidden" name="from" value=name.clone()/>
                        <input
                            type="text"
                            name="to"
                            required=true
                            placeholder={t!("card.rename_placeholder")}
                            value=name.clone()
                        />
                        <button type="submit" class="link-btn">{t!("card.rename_save")}</button>
                    </form>
                </details>
                <span class="actions-sep" aria-hidden="true"></span>
                <form
                    method="POST"
                    action="/api/rm"
                    class="inline-form card-delete"
                    onsubmit=file_confirm_js
                >
                    <input type="hidden" name="path" value=name.clone()/>
                    <input type="hidden" name="return_to" value=return_to.clone()/>
                    <button type="submit" class="link-btn link-btn-danger">{t!("file.delete")}</button>
                </form>
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

/// Render a Unix-epoch timestamp as `YYYY-MM-DD HH:MM` (UTC). Returns
/// `—` for the legacy-zero sentinel. No `chrono` / `time` dependency:
/// arithmetic via Howard Hinnant's days↔civil algorithm, which is
/// ~30 lines of pure integer math and exact for the Gregorian range we
/// care about (epoch → year 9999).
pub(crate) fn format_unix_utc(t: u64) -> String {
    if t == 0 {
        return "—".to_string();
    }
    let total = t;
    let hour = (total / 3600) % 24;
    let minute = (total / 60) % 60;
    let days = (total / 86400) as i64;
    // Howard Hinnant — civil_from_days; see http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y_pre = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y_pre + 1 } else { y_pre };
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}

/// Render a shard-count as a compact human-readable size proxy. Each
/// systematic shard at L0 stores roughly the chunk's bytes — the exact
/// bytes are layer-dependent (we don't track `sym_len` in the
/// view-model), so we treat the count as the user-facing weight. `0`
/// renders as `—` (typically directory markers).
pub(crate) fn format_shards(n_shards: u32) -> String {
    if n_shards == 0 {
        "—".to_string()
    } else {
        format!("{n_shards} shards")
    }
}

/// Human-readable storage size — the review flagged "N shards" as an
/// opaque number for a user trying to eyeball file sizes. We route
/// through this on every ObjectCard / tree leaf. `0` renders `—` so
/// directory markers stay tidy.
pub(crate) fn format_bytes(n: u64) -> String {
    if n == 0 {
        return "—".to_string();
    }
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if n < KIB {
        format!("{n} B")
    } else if n < MIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else if n < GIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    }
}

