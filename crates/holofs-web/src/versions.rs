//! `/versions/<name>` — per-object version history.
//!
//! Server-rendered list of every archived prior manifest for the named
//! catalog entry, newest first. Each row shows a timestamp + the short
//! `data_cid` + a "restore" form that POSTs to `/api/restore`. Versions
//! exist only when the server was booted with `--enable-versions`; the
//! page renders a friendly note otherwise.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::t;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VersionRow {
    pub id: String,
    pub created_at_ms: u64,
    pub cid_short: String,
    pub width: u32,
    pub height: u32,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct VersionListView {
    pub name: String,
    pub versions: Vec<VersionRow>,
    /// `true` when the gateway was booted with `--enable-versions`.
    pub enabled: bool,
}

#[server(
    name = ListVersions,
    prefix = "/api",
    endpoint = "versions_list",
)]
pub async fn list_versions(name: String) -> Result<VersionListView, ServerFnError> {
    use std::sync::Arc;
    let gw = expect_context::<Arc<holofs_gateway::Gateway>>();
    let enabled = gw.versions_enabled().await;
    if !enabled {
        return Ok(VersionListView {
            name,
            versions: Vec::new(),
            enabled: false,
        });
    }
    let raw = gw
        .list_versions(&name)
        .await
        .map_err(|e| ServerFnError::<server_fn::error::NoCustomError>::ServerError(e.to_string()))?;
    Ok(VersionListView {
        name,
        versions: raw
            .into_iter()
            .map(|v| VersionRow {
                id: v.id,
                created_at_ms: v.created_at_ms,
                cid_short: v.cid_short,
                width: v.width,
                height: v.height,
                kind: match v.kind {
                    holofs_model::manifest::ObjectKind::Image => "image".into(),
                    holofs_model::manifest::ObjectKind::Audio => "audio".into(),
                    holofs_model::manifest::ObjectKind::Text => "text".into(),
                    holofs_model::manifest::ObjectKind::Opaque => "opaque".into(),
                    holofs_model::manifest::ObjectKind::Directory => "directory".into(),
                },
            })
            .collect(),
        enabled: true,
    })
}

/// `GET /versions/<name>`.
#[component]
pub fn VersionsPage() -> impl IntoView {
    let params = use_params_map();
    let name = move || params.with(|p| p.get("name").unwrap_or_default());

    let data = Resource::new(name, |n| async move {
        if n.is_empty() {
            Err(ServerFnError::<server_fn::error::NoCustomError>::ServerError(
                "missing name".into(),
            ))
        } else {
            list_versions(n).await
        }
    });

    view! {
        <crate::ui::Topbar active="catalog"/>
        <main class="container">
            <Suspense fallback=move || view! { <p class="mut">{t!("versions.loading")}</p> }>
                {move || data.get().map(|res| match res {
                    Ok(v) => view! { <VersionsBody data=v/> }.into_any(),
                    Err(e) => view! {
                        <p class="bad">{t!("generic.load_failed")} " " {e.to_string()}</p>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn VersionsBody(data: VersionListView) -> impl IntoView {
    let VersionListView {
        name,
        versions,
        enabled,
    } = data;
    let enc = crate::url_encode(&name);
    let object_href = format!("/{enc}");
    let header_name = name.clone();

    view! {
        <p class="mut">
            <a href="/" rel="external">"← " {t!("generic.back_to_catalog")}</a>
        </p>
        <h2 class="versions-h">
            {t!("versions.title_prefix")} " "
            <a href=object_href rel="external">{header_name}</a>
        </h2>

        {if !enabled {
            view! {
                <p class="bad versions-disabled">{t!("versions.disabled")}</p>
            }.into_any()
        } else if versions.is_empty() {
            view! {
                <p class="mut">{t!("versions.empty")}</p>
            }.into_any()
        } else {
            let n_versions = versions.len();
            let name_for_form = name.clone();
            view! {
                <p class="mut">
                    {n_versions.to_string()} " " {t!("versions.count_suffix")}
                </p>
                <p class="mut versions-blurb">{t!("versions.blurb")}</p>
                <table class="versions-table">
                    <tr>
                        <th class="name">{t!("versions.col.when")}</th>
                        <th class="name">{t!("versions.col.kind")}</th>
                        <th class="name">{t!("versions.col.size")}</th>
                        <th class="name">{t!("versions.col.cid")}</th>
                        <th class="name">{t!("versions.col.actions")}</th>
                    </tr>
                    {versions.into_iter().map(|v| {
                        let when = format_ms_utc(v.created_at_ms);
                        let size = if v.width > 0 && v.height > 0 {
                            format!("{}×{}", v.width, v.height)
                        } else {
                            "-".into()
                        };
                        let confirm_js = format!(
                            "return confirm('{}');",
                            t!("versions.restore_confirm").replace('\'', "\\'")
                        );
                        view! {
                            <tr>
                                <td class="name"><code>{when}</code></td>
                                <td class="name"><code>{v.kind}</code></td>
                                <td class="name"><code>{size}</code></td>
                                <td class="name"><code>{v.cid_short}</code></td>
                                <td class="name">
                                    <form
                                        method="POST"
                                        action="/api/restore"
                                        class="inline-form"
                                        onsubmit=confirm_js
                                    >
                                        <input type="hidden" name="name" value=name_for_form.clone()/>
                                        <input type="hidden" name="id" value=v.id/>
                                        <button type="submit" class="link-btn">
                                            {t!("versions.action.restore")}
                                        </button>
                                    </form>
                                </td>
                            </tr>
                        }
                    }).collect_view()}
                </table>
            }.into_any()
        }}
    }
}

fn format_ms_utc(ms: u64) -> String {
    if ms == 0 {
        return "—".into();
    }
    let secs = (ms / 1000) as i64;
    // Cheap UTC formatter, no chrono dep. Same approach as
    // format_unix_utc in lib.rs but expressed locally for this module.
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let s = secs.rem_euclid(60);
    let total_min = secs.div_euclid(60);
    let mi = total_min.rem_euclid(60);
    let total_hours = total_min.div_euclid(60);
    let h = total_hours.rem_euclid(24);
    let mut days = total_hours.div_euclid(24);
    let mut y: i32 = 1970;
    while days >= days_in_year(y) {
        days -= days_in_year(y);
        y += 1;
    }
    let mut mo: u32 = 1;
    while days >= days_in_month(y, mo) {
        days -= days_in_month(y, mo);
        mo += 1;
    }
    (y, mo, (days + 1) as u32, h as u32, mi as u32, s as u32)
}

fn days_in_year(y: i32) -> i64 {
    if is_leap(y) {
        366
    } else {
        365
    }
}

fn days_in_month(y: i32, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
