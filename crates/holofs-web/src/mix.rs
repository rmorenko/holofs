//! `/mix?a=&b=&split=` — wavelet-mix page.
//!
//! Three URL params drive the page:
//! * `a` — source A (locked once the user navigated here from a
//!   catalog row).
//! * `b` — optional partner; absent on first paint, populated when the
//!   user picks one and submits the form.
//! * `split` — optional DWT split layer; numeric `0..=nlayers-1`.
//!
//! When both `b` and `split` are present, the page renders a live
//! preview `<img src="/api/mix.png?a=&b=&split=">` and a "save as…"
//! form that POSTs to `/api/mix-save`. The save handler ingests the
//! result and 303-redirects back to the catalog with `?open=` so the
//! new entry is immediately visible.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::i18n::current_locale;
use crate::t;
use crate::{get_catalog, url_encode, CatalogEntry};

/// Page component for `/mix`.
#[component]
pub fn MixPage() -> impl IntoView {
    let query = use_query_map();
    let params = move || {
        query.with(|q| {
            (
                q.get("a").unwrap_or_default(),
                q.get("b").unwrap_or_default(),
                q.get("split").and_then(|s| s.parse::<u8>().ok()),
            )
        })
    };

    // Catalog snapshot for the B dropdown — filtered to images only,
    // plus skip A itself. Compatibility (same shape/k/nlayers) isn't
    // checked here; if the user picks an incompatible B the preview
    // image fails to load and the server returns 400.
    let catalog = Resource::new(
        || (),
        |()| async move { get_catalog(String::new(), String::new(), String::new()).await },
    );

    view! {
        <crate::ui::Topbar active="catalog"/>
        <main class="container">
            {move || {
                let (a, b, split) = params();
                if a.is_empty() {
                    view! {
                        <p class="bad">{t!("mix.missing_a")}</p>
                        <p><a href="/">{t!("generic.back_to_catalog")} " →"</a></p>
                    }.into_any()
                } else {
                    view! {
                        <MixBody
                            a=a
                            b=b
                            split=split
                            catalog=catalog
                        />
                    }.into_any()
                }
            }}
        </main>
    }
}

#[component]
fn MixBody(
    a: String,
    b: String,
    split: Option<u8>,
    catalog: Resource<Result<Vec<CatalogEntry>, ServerFnError>>,
) -> impl IntoView {
    let enc_a = url_encode(&a);
    let a_for_title = a.clone();
    let a_for_form = a.clone();
    let a_for_preview = a.clone();
    let a_for_default = a.clone();

    view! {
        <p class="mut">
            <a href="/" rel="external">"← " {t!("generic.back_to_catalog")}</a>
        </p>
        <h2 style="margin-top:0">
            {t!("mix.title_prefix")} " "
            <a href={format!("/{enc_a}")} rel="external">{a_for_title}</a>
        </h2>
        <p class="mut">{t!("mix.intro")}</p>

        <Suspense fallback=move || view! { <p class="mut">{t!("catalog.loading")}</p> }>
            {move || catalog.get().map(|res| match res {
                Ok(list) => {
                    // Filter: images only, excluding `a` itself.
                    let a_match = a_for_form.clone();
                    let candidates: Vec<CatalogEntry> = list
                        .into_iter()
                        .filter(|e| e.kind == "image" && e.name != a_match)
                        .collect();
                    let a_form = a_for_form.clone();
                    let b_form = b.clone();
                    let split_form = split.unwrap_or(0);
                    let default_dest = build_default_dest(&a_for_default, &b, split);
                    view! {
                        <form
                            class="mix-form"
                            method="GET"
                            action="/mix"
                        >
                            <input type="hidden" name="a" value=a_form.clone()/>
                            <label class="mix-field mix-field-wide">
                                <span class="lbl">
                                    {t!("mix.field.b")}
                                    " "
                                    <span class="mut">
                                        "(" {candidates.len().to_string()} ")"
                                    </span>
                                </span>
                                // .1: text input + datalist instead
                                // of a `<select>`. Typing filters the list
                                // natively in the browser — no JS — so
                                // picking from hundreds of images stays
                                // workable.
                                <input
                                    type="text"
                                    name="b"
                                    list="mix-b-options"
                                    placeholder={t!("mix.pick_b")}
                                    value=b_form.clone()
                                    required=true
                                    autocomplete="off"
                                    spellcheck="false"
                                />
                                <datalist id="mix-b-options">
                                    {candidates.into_iter().map(|c| {
                                        let value = c.name;
                                        view! { <option value=value/> }
                                    }).collect_view()}
                                </datalist>
                            </label>
                            <label class="mix-field">
                                <span class="lbl">{t!("mix.field.split")}</span>
                                <input
                                    type="number"
                                    name="split"
                                    min="0"
                                    max="7"
                                    value=split_form.to_string()
                                    required=true
                                />
                            </label>
                            <button type="submit" class="tree-control-btn">
                                {t!("mix.apply")}
                            </button>
                        </form>

                        {(!b.is_empty() && split.is_some()).then(|| view! {
                            <MixResult
                                a=a_for_preview.clone()
                                b=b.clone()
                                split=split.unwrap()
                                default_dest=default_dest
                            />
                        })}
                        <p style="margin-top:24px">
                            <a href="/" rel="external">
                                "← " {t!("generic.back_to_catalog")}
                            </a>
                        </p>
                    }.into_any()
                }
                Err(e) => view! {
                    <p class="bad">{t!("catalog.load_failed")} " " {e.to_string()}</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn MixResult(a: String, b: String, split: u8, default_dest: String) -> impl IntoView {
    let enc_a = url_encode(&a);
    let enc_b = url_encode(&b);
    let preview_src = format!("/api/mix.png?a={enc_a}&b={enc_b}&split={split}");
    let a_form = a.clone();
    let b_form = b.clone();
    view! {
        <section class="mix-result">
            <h3>{t!("mix.preview_h")}</h3>
            <div class="mix-preview">
                <img
                    src=preview_src
                    alt={t!("mix.preview_alt")}
                    loading="lazy"
                />
            </div>
            <form
                class="mix-save"
                method="POST"
                action="/api/mix-save"
            >
                <input type="hidden" name="a" value=a_form/>
                <input type="hidden" name="b" value=b_form/>
                <input type="hidden" name="split" value=split.to_string()/>
                <label class="mix-field">
                    <span class="lbl">{t!("mix.field.dest")}</span>
                    <input
                        type="text"
                        name="dest"
                        value=default_dest
                        placeholder="hybrid.png"
                        required=true
                    />
                </label>
                <button type="submit" class="tree-control-btn">
                    {t!("mix.save")}
                </button>
            </form>
        </section>
    }
}

/// Build a default "save as" name based on the two sources and split.
/// `mandala.png` + `photo.png` split=2 → `mix_mandala_photo_s2.png`.
fn build_default_dest(a: &str, b: &str, split: Option<u8>) -> String {
    fn stem(path: &str) -> String {
        let leaf = path.rsplit('/').next().unwrap_or(path);
        let stem = leaf.rsplit_once('.').map(|(s, _)| s).unwrap_or(leaf);
        stem.replace('/', "_")
    }
    let _ = current_locale();
    match split {
        Some(s) => format!("mix_{}_{}_s{s}.png", stem(a), stem(b)),
        None => format!("mix_{}_{}.png", stem(a), stem(b)),
    }
}
