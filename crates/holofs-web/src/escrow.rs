//! `GET /escrow` — Leptos SSR page with split + recover forms.
//!
//! POST endpoints (`/escrow/split`, `/escrow/recover`) and binary download
//! (`/escrow/download/<id>_<idx>.holoshare`) live in
//! [`crate::handlers`] as plain axum handlers — they deal with multipart
//! parsing and binary I/O, not HTML.

use leptos::prelude::*;

use crate::i18n::current_locale;
use crate::t;
use crate::ui::Topbar;

/// `GET /escrow` — static UI: introduces escrow, hosts split + recover forms.
#[component]
pub fn EscrowPage() -> impl IntoView {
    // pass the active locale into the form so the
    // server-rendered split-result page can translate its strings. We
    // capture `current_locale()` at render time — switching locales
    // means re-rendering this page anyway, so a snapshot is fine.
    let lang = current_locale();
    let lang_split = lang.clone();
    let lang_recover = lang.clone();
    view! {
        <Topbar active="escrow"/>

        <main class="container">
            <h2>{t!("escrow.title")}</h2>
            <p>{t!("escrow.intro_p1")}</p>
            <p class="mut">{t!("escrow.intro_p2")}</p>

            // custom-styled file inputs replace the native
            // controls so the "Choose file" button text follows the page
            // locale (native picker uses the OS language) and the form
            // accepts drag-and-drop. Without JS the visually-hidden
            // input still works — `<label>` opens the picker natively.
            <section class="escrow-section">
                <h3>{t!("escrow.split_h")}</h3>
                <p class="drop-prompt mut">{t!("escrow.drop_file")}</p>
                <form
                    method="POST"
                    action="/escrow/split"
                    enctype="multipart/form-data"
                    class="escrow-form"
                >
                    <input type="hidden" name="lang" value=lang_split/>
                    <label class="file-label">
                        <input type="file" name="file" required=true/>
                        <span class="file-button">{t!("escrow.choose_file")}</span>
                        <span class="file-name mut">{t!("escrow.no_file")}</span>
                    </label>
                    <label>
                        <span class="lbl">{t!("escrow.label.k")}</span>
                        <input type="number" name="k" value="3" min="1" max="64" required=true/>
                    </label>
                    <label>
                        <span class="lbl">{t!("escrow.label.n")}</span>
                        <input type="number" name="n" value="5" min="1" max="64" required=true/>
                    </label>
                    <button type="submit">{t!("escrow.btn.split")}</button>
                </form>
            </section>

            <section class="escrow-section">
                <h3>{t!("escrow.recover_h")}</h3>
                <p class="drop-prompt mut">{t!("escrow.drop_shares")}</p>
                <form
                    method="POST"
                    action="/escrow/recover"
                    enctype="multipart/form-data"
                    class="escrow-form"
                >
                    <input type="hidden" name="lang" value=lang_recover/>
                    <label class="file-label">
                        <input type="file" name="shares" multiple=true required=true/>
                        <span class="file-button">{t!("escrow.choose_shares")}</span>
                        <span
                            class="file-name mut"
                            data-multi-template={t!("escrow.files_chosen")}
                        >
                            {t!("escrow.no_shares")}
                        </span>
                    </label>
                    <p class="hint mut">{t!("escrow.label.shares")}</p>
                    <button type="submit">{t!("escrow.btn.recover")}</button>
                </form>
            </section>

            <script defer="defer" src="/assets/escrow-init.js"></script>
        </main>
    }
}

/// split-result body rendered through the same component
/// system as the rest of the site. The `lang` value is provided into
/// Leptos context by the caller (`handlers::escrow_split`), so every
/// `t!()` inside this view picks up the right locale.
///
/// We render a **stripped-down** topbar inline (no `<LocaleSwitcher>`)
/// because this response page lives outside the Leptos router — the
/// router-aware components (`Topbar`'s `LocaleSwitcher`, anything
/// reaching for `use_location()`) would panic with "Tried to access
/// Location outside a <Router>". The user lands here via a one-shot
/// POST; the back-link takes them back to `/escrow` where the full
/// chrome lives again.
#[component]
pub fn EscrowSplitResultView(
    filename: String,
    source_bytes: u64,
    k: usize,
    n: usize,
    escrow_id_hex: String,
    shares: Vec<EscrowShareRow>,
) -> impl IntoView {
    use crate::i18n::translate;
    let id_preview: String = escrow_id_hex.chars().take(16).collect();
    let loc = current_locale();
    // {n} / {k} are placeholders in the localized title; substitute
    // before rendering since `t!()` returns a static borrow.
    let title = translate("escrow.result.title", &loc)
        .replace("{n}", &n.to_string())
        .replace("{k}", &k.to_string());
    view! {
        <header class="topbar">
            <h1>"holofs"</h1>
            <nav>
                <a href="/">{t!("nav.catalog")}</a>
                <a href="/health">{t!("nav.health")}</a>
                <a href="/escrow" class="active">{t!("nav.escrow")}</a>
                <a href="/help">{t!("nav.help")}</a>
            </nav>
        </header>
        <main class="container">
            <h2>{title}</h2>
            <p>
                {t!("escrow.result.source")} " " <code>{filename}</code>
                " · " {source_bytes} " B · " {t!("escrow.result.id")}
                " " <code>{id_preview}</code>
            </p>
            <p class="mut">{t!("escrow.result.warning")}</p>
            <table>
                <thead>
                    <tr>
                        <th>{t!("escrow.result.col.idx")}</th>
                        <th class="name">{t!("escrow.result.col.file")}</th>
                        <th>{t!("escrow.result.col.size")}</th>
                        // Download-link column. Body uses `class="name"`
                        // for left-alignment; pair the empty header so
                        // it doesn't visually drift.
                        <th class="name"></th>
                    </tr>
                </thead>
                <tbody>
                    {shares.into_iter().map(|s| view! {
                        <tr>
                            <td>{s.idx}</td>
                            <td class="name"><code>{s.filename.clone()}</code></td>
                            <td>{s.bytes} " B"</td>
                            <td class="name">
                                <a
                                    href={format!("/escrow/download/{}", s.download_path)}
                                    download=s.filename.clone()
                                    // `/escrow/download/...` is an axum-only route,
                                    // not in the leptos Route table. Without
                                    // rel="external" the SPA router intercepts the
                                    // click and shows the "not found" fallback.
                                    rel="external"
                                >
                                    {t!("escrow.result.action.download")}
                                </a>
                            </td>
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
            <p><a href="/escrow">{t!("escrow.result.back")}</a></p>
        </main>
    }
}

/// One row of [`EscrowSplitResultView`]. Mirrors `EscrowShareInfo`
/// from `holofs-gateway` but stripped to the fields the view actually
/// reads — keeps the view component free of an `ssr`-only dep.
#[derive(Clone, Debug)]
pub struct EscrowShareRow {
    pub idx: usize,
    pub filename: String,
    pub bytes: u64,
    pub download_path: String,
}
