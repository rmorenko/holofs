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
    /// One of `"image"`, `"audio"`, `"text"`, `"opaque"`, `"directory"`.
    pub kind: String,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
    pub n_shards: u32,
    pub cid_short: String,
    pub audio_sample_rate: u32,
    pub channels: u8,
    /// Unix epoch seconds when this entry was added to the catalog. `0`
    /// means "unknown" — typically a legacy manifest written under
    /// `HOLOFSM6` or `HOLOFSM7`, which had no timestamp.
    pub created_at_unix: u64,
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
pub async fn get_catalog(
    name_glob: String,
    date_from: String,
    date_to: String,
) -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let catalog = gw.catalog().lock().await;
    let filter = CatalogFilter::parse(&name_glob, &date_from, &date_to)
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e))?;
    let raw: Vec<CatalogEntry> = catalog
        .entries
        .iter()
        .map(|(name, m)| CatalogEntry::from_manifest(name, m))
        .collect();
    // Stage 11.17: apply server-side filter. Filter is matched against
    // leaves (non-directory entries); ancestor directories of any
    // matching leaf are kept automatically so the tree still has paths
    // to render. Directories whose own name/date matches the filter
    // also stay (so users can `q=Roman*` and find that folder
    // directly).
    let mut out: Vec<CatalogEntry> = if filter.is_empty() {
        raw
    } else {
        apply_filter(raw, &filter)
    };
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
pub async fn list_dir(
    prefix: String,
    name_glob: String,
    date_from: String,
    date_to: String,
) -> Result<Vec<CatalogEntry>, ServerFnError> {
    use std::sync::Arc;

    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let children = gw
        .list_dir(&prefix)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    let filter = CatalogFilter::parse(&name_glob, &date_from, &date_to)
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e))?;
    let mut out: Vec<CatalogEntry> = children
        .into_iter()
        .map(|(name, m)| CatalogEntry::from_manifest(&name, &m))
        .filter(|e| filter.is_empty() || filter.matches(e))
        .collect();
    // Directories first, then objects — both alphabetical inside the bucket.
    out.sort_by(|a, b| {
        let a_dir = a.kind == "directory";
        let b_dir = b.kind == "directory";
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Server-side filter shared by `get_catalog` + `list_dir`. Built from
/// three URL params: `q` (name glob with `*`), `from` and `to` (date
/// range, `YYYY-MM-DD`).
#[cfg(feature = "ssr")]
#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogFilter {
    /// Compiled regex from the glob pattern. `None` means "no name
    /// filter active".
    name_re: Option<regex::Regex>,
    /// Lower bound on `created_at_unix`. `0` = no lower bound.
    from_unix: u64,
    /// Upper bound (exclusive). `0` = no upper bound.
    to_unix_exclusive: u64,
}

#[cfg(feature = "ssr")]
impl CatalogFilter {
    /// Parse raw query params; reject malformed dates / regexes with a
    /// user-readable message.
    fn parse(name_glob: &str, from: &str, to: &str) -> Result<Self, String> {
        let name_re = if name_glob.trim().is_empty() {
            None
        } else {
            Some(compile_glob(name_glob.trim()).map_err(|e| format!("bad name filter: {e}"))?)
        };
        let from_unix = parse_date_to_unix(from, false)
            .map_err(|e| format!("bad `from` date: {e}"))?;
        let to_unix_exclusive = parse_date_to_unix(to, true)
            .map_err(|e| format!("bad `to` date: {e}"))?;
        Ok(Self {
            name_re,
            from_unix,
            to_unix_exclusive,
        })
    }

    fn is_empty(&self) -> bool {
        self.name_re.is_none() && self.from_unix == 0 && self.to_unix_exclusive == 0
    }

    /// True when `entry` clears every active sub-filter. Legacy
    /// entries with `created_at_unix == 0` (HOLOFSM6/7) are kept
    /// whenever a date filter is set — without a timestamp we'd have
    /// to drop them, which is more confusing than including them.
    fn matches(&self, entry: &CatalogEntry) -> bool {
        if let Some(re) = &self.name_re {
            // Glob runs against the **basename** so `q=*.png` doesn't
            // need to know what folder the file is in. Users who want
            // path-aware matches can prefix `*/`: `*/2026/*` for an
            // explicit directory segment.
            let leaf = entry.name.rsplit('/').next().unwrap_or(&entry.name);
            if !re.is_match(leaf) {
                return false;
            }
        }
        let has_ts = entry.created_at_unix != 0;
        if self.from_unix != 0 && has_ts && entry.created_at_unix < self.from_unix {
            return false;
        }
        if self.to_unix_exclusive != 0 && has_ts && entry.created_at_unix >= self.to_unix_exclusive
        {
            return false;
        }
        true
    }
}

/// Compile a simple glob (`*` = any sequence) into a full-string regex.
/// Every other char is regex-escaped. Patterns are anchored on both
/// sides (`^…$`) so `photo` matches exactly `photo`, not `photograph`.
#[cfg(feature = "ssr")]
fn compile_glob(pat: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::with_capacity(pat.len() * 2 + 4);
    out.push('^');
    for c in pat.chars() {
        match c {
            '*' => out.push_str(".*"),
            // Regex metacharacters that must be escaped to keep their
            // literal meaning inside the user's filter pattern.
            '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    regex::RegexBuilder::new(&out).case_insensitive(true).build()
}

/// Parse `YYYY-MM-DD` to a Unix epoch second. Empty string is a
/// no-op (`0`). `inclusive_end=true` flips the meaning to "end of day"
/// so the upper bound covers the entire `to` day — the filter then
/// stores the value as **exclusive** (start of next day).
#[cfg(feature = "ssr")]
fn parse_date_to_unix(s: &str, inclusive_end: bool) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    // Expect strict YYYY-MM-DD; the HTML5 `<input type="date">` always
    // emits this format.
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("expected YYYY-MM-DD, got {s:?}"));
    }
    let y: i64 = parts[0].parse().map_err(|_| format!("bad year in {s:?}"))?;
    let m: u32 = parts[1].parse().map_err(|_| format!("bad month in {s:?}"))?;
    let d: u32 = parts[2].parse().map_err(|_| format!("bad day in {s:?}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=2999).contains(&y) {
        return Err(format!("out-of-range date {s:?}"));
    }
    let mut secs = ymd_to_unix(y, m, d);
    if inclusive_end {
        // Advance by 86 400s so a `to=2026-06-23` filter covers
        // anything created up to 2026-06-23 23:59:59 UTC.
        secs = secs.saturating_add(86_400);
    }
    Ok(secs)
}

/// Civil-date → Unix-epoch seconds (UTC). Pure arithmetic, no chrono
/// dependency — Howard Hinnant's `days_from_civil`. All intermediates
/// stay signed so the `m_adj = m + 9` / `m - 3` flip can't wrap when
/// `m <= 2`.
#[cfg(feature = "ssr")]
fn ymd_to_unix(y: i64, m: u32, d: u32) -> u64 {
    let m = m as i64;
    let d = d as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // 0..=399
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146_097 + doe - 719_468;
    days_since_epoch.max(0) as u64 * 86_400
}

/// Tree-aware filter. Returns the input minus entries that don't match
/// the filter, **plus** every ancestor-directory entry of any retained
/// leaf so the path is still navigable. Folders that match the filter
/// directly are included regardless of whether they have surviving
/// children.
#[cfg(feature = "ssr")]
fn apply_filter(entries: Vec<CatalogEntry>, filter: &CatalogFilter) -> Vec<CatalogEntry> {
    use std::collections::{HashMap, HashSet};

    let mut by_name: HashMap<String, CatalogEntry> = HashMap::with_capacity(entries.len());
    for e in entries.into_iter() {
        by_name.insert(e.name.clone(), e);
    }

    let mut keep: HashSet<String> = HashSet::new();
    for (name, entry) in by_name.iter() {
        if filter.matches(entry) {
            keep.insert(name.clone());
            // Pull every ancestor segment so the tree path survives.
            let mut cur = name.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                if parent.is_empty() {
                    break;
                }
                if !keep.insert(parent.to_string()) {
                    // Parent already retained — its ancestors are too.
                    break;
                }
                cur = parent;
            }
        }
    }

    let mut out: Vec<CatalogEntry> =
        keep.into_iter().filter_map(|k| by_name.remove(&k)).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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
            created_at_unix: m.created_at_unix,
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
                // Stage 11.13: pre-paint theme bootstrap. Reads the user's
                // last choice from localStorage and sets `data-theme` on
                // <html> before the CSS paints — no flash on reload.
                <script>{
                    "(function(){try{var t=localStorage.getItem('holofs-theme')||'dark';\
                    document.documentElement.dataset.theme=t;}catch(e){}})();"
                }</script>
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

/// `GET /` — catalog entry point. Two modes:
///
/// - `/` (no `?p=`): **tree-view** of the entire catalog with
///   collapsible `<details>` nodes. Read-only — no upload / mkdir forms.
///   Each folder header carries an `[open →]` link to the focus view.
/// - `/?p=<path>`: **focus view** of a single directory with breadcrumb,
///   upload + mkdir forms and per-tile actions. This is where creates
///   and deletes happen.
#[component]
fn CatalogPage() -> impl IntoView {
    let query = use_query_map();
    let prefix_signal = move || query.with(|q| q.get("p").unwrap_or_default());

    view! {
        <ui::Topbar active="catalog"/>

        <main class="container">
            {move || {
                let prefix = prefix_signal();
                if prefix.is_empty() {
                    view! { <CatalogTreeView/> }.into_any()
                } else {
                    view! { <CatalogFocusView prefix=prefix/> }.into_any()
                }
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
    }
}

/// Stage 11.17: server-side catalog filter bar. Renders a GET form that
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
        // Stage 11.18: progressive enhancement — flatpickr replaces the
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
            // Stage 11.17 follow-up: pin `lang` on the date inputs so the
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

/// Stable sort key for tree children. Driven by the `?sort=` query
/// param on the root view. `Name` is the default and always
/// alphabetical; `Size` ranks by `n_shards`; `Kind` groups by
/// ObjectKind label; `Date` ranks by Stage-11.12 `created_at_unix`
/// (newest first; legacy `0` floats to the bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeSort {
    Name,
    Size,
    Kind,
    Date,
}

impl TreeSort {
    fn from_param(s: &str) -> Self {
        match s {
            "size" => Self::Size,
            "kind" => Self::Kind,
            "date" => Self::Date,
            _ => Self::Name,
        }
    }
    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Kind => "kind",
            Self::Date => "date",
        }
    }
}

/// Full-catalog tree view. Fetches every entry, groups by parent path,
/// and emits a nested `<details>` / `<summary>` tree. The native
/// browser element handles expand / collapse — no JS, no hydration
/// dance, no client state. Each folder summary shows the child count
/// and an `[open]` link back to the focus view for create / manage
/// actions. Sort key comes from `?sort=name|size|kind`.
#[component]
fn CatalogTreeView() -> impl IntoView {
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
    let all = Resource::new(filter_signal, |(q, f, t)| async move {
        get_catalog(q, f, t).await
    });

    view! {
        <p class="mut tree-intro">{t!("catalog.tree_intro")}</p>
        <FilterBar prefix=String::new()/>
        <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
            {move || {
                let sort = sort_signal();
                all.get().map(|res| match res {
                    Ok(list) if list.is_empty() => view! {
                        <p class="empty-state">
                            {t!("catalog.empty")} " " {t!("catalog.empty_hint")} " "
                            <code>"curl -X PUT http://<host>/<name>"</code>
                        </p>
                    }.into_any(),
                    Ok(list) => view! { <CatalogTreeBody entries=list sort=sort/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })
            }}
        </Suspense>
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
                onclick="document.querySelectorAll('.tree-root details').forEach(d => d.open = true)"
            >
                <span class="tree-control-icon">"⊕"</span>
                <span class="tree-control-label">{t!("tree.expand_all")}</span>
            </button>
            <button
                type="button"
                class="tree-control-btn tree-control-collapse"
                onclick="document.querySelectorAll('.tree-root details').forEach(d => d.open = false)"
            >
                <span class="tree-control-icon">"⊖"</span>
                <span class="tree-control-label">{t!("tree.collapse_all")}</span>
            </button>
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
                    // "unknown" (legacy manifest before Stage 11.12) and
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
        let size_str = format_shards(entry.n_shards);
        let date_str = format_unix_utc(entry.created_at_unix);
        let icon: &'static str = match kind.as_str() {
            "image" => "🖼",
            "audio" => "🎵",
            "text" => "📝",
            _ => "📦",
        };
        // Stage 11.17: confirm dialog before the form POSTs. The
        // translated string is interpolated raw so we double single
        // quotes to keep it inside the JS string literal.
        let file_confirm_js = format!(
            "return confirm('{}');",
            t!("file.delete_confirm").replace('\'', "\\'")
        );
        view! {
            <li class={format!("tree-leaf kind-{kind}")}>
                <span class="tree-icon">{icon}</span>
                <a class="tree-name" href={format!("/{enc_full}")}>{basename}</a>
                <span class="tree-meta">
                    <span class="tree-meta-size" title="shard count">{size_str}</span>
                    <span class="tree-meta-date" title="created (UTC)">{date_str}</span>
                </span>
                <span class="tree-sep">"·"</span>
                <span class="tree-actions">
                    {(kind == "image" || kind == "audio").then(|| view! {
                        <a href={format!("/preview/{}", enc_full.clone())}>"preview"</a>
                        <span class="tree-sep">"·"</span>
                    })}
                    <a href={format!("/inspect/{}", enc_full.clone())}>"shards"</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/similar/{}", enc_full.clone())}>{t!("card.action.similar")}</a>
                    <span class="tree-sep">"·"</span>
                    <a href={format!("/health/{}", enc_full.clone())}>{t!("card.action.health")}</a>
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
                            <form
                                method="POST"
                                action="/api/mkdir"
                                class="inline-form tree-inline-mkdir"
                                onclick="event.stopPropagation()"
                            >
                                <input type="hidden" name="parent" value=path.clone()/>
                                <input type="hidden" name="return_to" value="/"/>
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
                            <a href={format!("/?p={enc_path}")}>{t!("folder.open")} " →"</a>
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
        created_at_unix: _,
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
                " · "
                <form
                    method="POST"
                    action="/api/rm"
                    class="inline-form"
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

#[cfg(all(test, feature = "ssr"))]
mod filter_tests {
    use super::*;

    fn entry(name: &str, kind: &str, created: u64) -> CatalogEntry {
        CatalogEntry {
            name: name.into(),
            kind: kind.into(),
            content_type: String::new(),
            width: 0,
            height: 0,
            n_shards: 0,
            cid_short: String::new(),
            audio_sample_rate: 0,
            channels: 0,
            created_at_unix: created,
        }
    }

    #[test]
    fn glob_compiles_to_anchored_regex() {
        let re = compile_glob("*.png").unwrap();
        assert!(re.is_match("photo.png"));
        assert!(re.is_match("a.b.png"));
        assert!(!re.is_match("photo.png.bak"));
    }

    #[test]
    fn glob_is_case_insensitive() {
        let re = compile_glob("*.PNG").unwrap();
        assert!(re.is_match("photo.png"));
        assert!(re.is_match("PHOTO.PNG"));
    }

    #[test]
    fn glob_escapes_regex_metachars() {
        let re = compile_glob("a.b").unwrap();
        assert!(re.is_match("a.b"));
        // The `.` is escaped, so it does NOT match any single char.
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn date_parse_ymd_to_unix_round_trips() {
        // 1970-01-01 = epoch 0.
        assert_eq!(parse_date_to_unix("1970-01-01", false).unwrap(), 0);
        // Inclusive-end flag adds one day (86 400 s).
        assert_eq!(parse_date_to_unix("1970-01-01", true).unwrap(), 86_400);
        // 2026-01-01 — sanity check against the JS Date equivalent
        // (Date.UTC(2026,0,1)/1000 = 1767225600).
        assert_eq!(parse_date_to_unix("2026-01-01", false).unwrap(), 1_767_225_600);
    }

    #[test]
    fn date_parse_rejects_malformed() {
        assert!(parse_date_to_unix("not-a-date", false).is_err());
        assert!(parse_date_to_unix("2026-13-01", false).is_err());
        assert!(parse_date_to_unix("2026-01-32", false).is_err());
        // Empty string is the "no bound" sentinel, not an error.
        assert_eq!(parse_date_to_unix("", false).unwrap(), 0);
    }

    #[test]
    fn filter_matches_combines_name_and_date() {
        let f = CatalogFilter::parse("*.png", "2026-01-01", "2026-12-31").unwrap();
        // Inside both bounds.
        assert!(f.matches(&entry("photo.png", "image", 1_770_000_000)));
        // Wrong extension.
        assert!(!f.matches(&entry("notes.txt", "text", 1_770_000_000)));
        // Before from.
        assert!(!f.matches(&entry("photo.png", "image", 1_000_000_000)));
    }

    #[test]
    fn filter_keeps_legacy_zero_timestamps() {
        // Legacy entries (HOLOFSM6/7) have created_at_unix = 0. They
        // pass any date filter — better to show them than hide them
        // silently.
        let f = CatalogFilter::parse("", "2026-01-01", "2026-12-31").unwrap();
        assert!(f.matches(&entry("anything.txt", "text", 0)));
    }

    #[test]
    fn glob_matches_basename_not_full_path() {
        let f = CatalogFilter::parse("photo*", "", "").unwrap();
        // Glob looks at the leaf, so `Roman/photo.png` should match.
        assert!(f.matches(&entry("Roman/photo.png", "image", 0)));
        assert!(f.matches(&entry("photo_gray.png", "image", 0)));
        assert!(!f.matches(&entry("Roman/notes.txt", "text", 0)));
    }

    #[test]
    fn apply_filter_keeps_ancestor_directories() {
        let mut entries = vec![
            entry("Roman", "directory", 0),
            entry("Roman/sub", "directory", 0),
            entry("Roman/sub/photo.png", "image", 0),
            entry("Roman/sub/notes.txt", "text", 0),
            entry("other.txt", "text", 0),
        ];
        // Glob keeps `*.png` only; ancestor dirs of the surviving leaf
        // ride along so the tree path is still navigable.
        let f = CatalogFilter::parse("*.png", "", "").unwrap();
        let mut kept = apply_filter(std::mem::take(&mut entries), &f);
        kept.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = kept.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Roman", "Roman/sub", "Roman/sub/photo.png"]);
    }
}
