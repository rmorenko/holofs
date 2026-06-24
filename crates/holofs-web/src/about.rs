//! `/about` — marketing / pitch page.
//!
//! Static (no server function), pure i18n strings. The point is to give
//! a non-engineer reader a one-screen answer to "why this and not S3".
//! Structure:
//!   * hero with the one-line pitch,
//!   * four architecture cards covering what only holofs gives you,
//!   * a bullet list of business outcomes,
//!   * a grid of real use-cases with body text,
//!   * a CTA back into the catalog.

use leptos::prelude::*;

use crate::t;

/// `GET /about`.
#[component]
pub fn AboutPage() -> impl IntoView {
    view! {
        <crate::ui::Topbar active="about"/>

        <main class="container about-page">
            // --- Hero -----------------------------------------------------
            <section class="about-hero">
                <p class="about-hero-tag">{t!("about.hero.tag")}</p>
                <h1 class="about-hero-h">{t!("about.hero.h")}</h1>
                <p class="about-hero-sub">{t!("about.hero.sub")}</p>
            </section>

            // --- Architecture cards --------------------------------------
            <h2 class="about-section-h">{t!("about.unique.h")}</h2>
            <section class="about-cards">
                <ArchCard
                    title_key="about.unique.layer.h"
                    body_key="about.unique.layer.body"
                />
                <ArchCard
                    title_key="about.unique.dedup.h"
                    body_key="about.unique.dedup.body"
                />
                <ArchCard
                    title_key="about.unique.rlnc.h"
                    body_key="about.unique.rlnc.body"
                />
                <ArchCard
                    title_key="about.unique.transforms.h"
                    body_key="about.unique.transforms.body"
                />
            </section>

            // --- Business outcomes (bullets) -----------------------------
            <h2 class="about-section-h">{t!("about.value.h")}</h2>
            <ValueList/>

            // --- Use-case grid -------------------------------------------
            <h2 class="about-section-h">{t!("about.cases.h")}</h2>
            <section class="about-cases">
                <CaseCard
                    title_key="about.cases.brand.h"
                    body_key="about.cases.brand.body"
                />
                <CaseCard
                    title_key="about.cases.dedup.h"
                    body_key="about.cases.dedup.body"
                />
                <CaseCard
                    title_key="about.cases.cdn.h"
                    body_key="about.cases.cdn.body"
                />
                <CaseCard
                    title_key="about.cases.plagiarism.h"
                    body_key="about.cases.plagiarism.body"
                />
                <CaseCard
                    title_key="about.cases.audio.h"
                    body_key="about.cases.audio.body"
                />
                <CaseCard
                    title_key="about.cases.escrow.h"
                    body_key="about.cases.escrow.body"
                />
            </section>

            // --- CTA back to catalog -------------------------------------
            <section class="about-cta">
                <h3>{t!("about.cta.h")}</h3>
                <p>{t!("about.cta.body")}</p>
                <p>
                    <a class="about-cta-btn" href="/" rel="external">
                        {t!("about.cta.button")}
                    </a>
                </p>
            </section>
        </main>
    }
}

/// One of the four "architecture" cards. Title + body are both i18n keys;
/// kept as `&'static str` since the table is comptime-static.
#[component]
fn ArchCard(title_key: &'static str, body_key: &'static str) -> impl IntoView {
    view! {
        <article class="about-card">
            <h3 class="about-card-h">{crate::i18n::translate(title_key, &crate::i18n::current_locale())}</h3>
            <p class="about-card-body">{crate::i18n::translate(body_key, &crate::i18n::current_locale())}</p>
        </article>
    }
}

/// Same shape as [`ArchCard`] but with a slightly louder accent in CSS.
#[component]
fn CaseCard(title_key: &'static str, body_key: &'static str) -> impl IntoView {
    view! {
        <article class="about-case">
            <h3 class="about-case-h">{crate::i18n::translate(title_key, &crate::i18n::current_locale())}</h3>
            <p class="about-case-body">{crate::i18n::translate(body_key, &crate::i18n::current_locale())}</p>
        </article>
    }
}

/// Renders `about.value.list` — a single i18n string of bullets separated
/// by `" | "`. Single string avoids ~6 separate keys per locale.
#[component]
fn ValueList() -> impl IntoView {
    let raw = crate::i18n::translate("about.value.list", &crate::i18n::current_locale());
    let items: Vec<String> = raw
        .split('|')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    view! {
        <ul class="about-value-list">
            {items.into_iter().map(|line| view! {
                <li>{line}</li>
            }).collect_view()}
        </ul>
    }
}
