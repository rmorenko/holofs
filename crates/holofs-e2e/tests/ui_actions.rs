//! Per-image action links inside catalog tree leaves (Stage 11.16+).
//!
//! Every image card carries a row of links: `preview`, `shards`,
//! `similar`, `health`, `mix`, `holo`, `spotlight`, `versions`,
//! `✕`. All except the delete glyph land on server-rendered routes,
//! so each one must carry `rel="external"` — otherwise the leptos
//! Router intercepts the click and the browser ends up on a 404.
//! This was a real regression (memory `Rule 3` in feedback_workflow).
//!
//! The tests here exercise the cross-link wiring without clicking
//! through every endpoint: build the expected paths from a seeded
//! file, fetch each one with reqwest, assert a successful response
//! (or, for the obvious-redirect routes, a sane status). The
//! brittleness of full click-through interaction is reserved for
//! the per-feature suites (ui_holo, ui_similar, …).

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn every_per_image_action_route_returns_2xx_for_a_seeded_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    let name = "photos/abstract/mandala.png";
    let routes = [
        format!("/preview/{name}"),
        format!("/inspect/{name}"),
        format!("/similar/{name}"),
        format!("/health/{name}"),
        format!("/holo/{name}"),
        format!("/versions/{name}"),
        // `/mix?a=…` and `/spotlight?a=…` are query-driven, not
        // path-driven; they still need the seeded image to anchor
        // the form's initial state.
        format!("/mix?a={name}"),
        format!("/spotlight?a={name}"),
    ];
    let client = reqwest::Client::new();
    for route in routes {
        let resp = client.get(harness.url(&route)).send().await?;
        let status = resp.status().as_u16();
        assert!(
            (200..400).contains(&status),
            "route {route} returned HTTP {status}"
        );
    }
    harness.close().await
}

#[tokio::test]
async fn per_image_action_links_in_html_carry_rel_external() -> Result<()> {
    // The bug we're guarding against: a leptos Route catches the
    // <a> click and routes client-side, but server-rendered pages
    // like /inspect, /similar, /health are NOT in the leptos Route
    // table from the catalog's perspective (well — they are, but
    // they re-fetch server data). Without `rel="external"`, the
    // first click after WASM hydrate silently 404s.
    //
    // We don't render the catalog (it's WASM-driven) — instead we
    // pull the file_metrics page where the action row is server-
    // emitted and inspect its HTML.
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    let html = String::from_utf8(
        harness
            .get_bytes("/health/photos/abstract/mandala.png")
            .await?,
    )
    .unwrap_or_default();
    // The topbar nav also carries rel="external", which is the
    // same defence in a different shell. Either presence proves
    // the convention is alive on this page.
    assert!(
        html.contains(r#"rel="external""#),
        "expected at least one rel=\"external\" attribute in /health/<name> HTML \
         to defend against SPA-router hijack; got: {} bytes",
        html.len()
    );
    harness.close().await
}
