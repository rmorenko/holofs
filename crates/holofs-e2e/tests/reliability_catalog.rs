//! Regression guard for the lazy-catalog `loading catalog…` hang
//! (commit `1ea1a9c`).
//!
//! Pre-fix `LazyDirNode` drove its child fetch via a manual
//! `Effect::new(move |_| { spawn_local(list_dir_page(...)) })`
//! pattern. Under leptos 0.7 streaming hydration the Effect's
//! initial pass didn't reliably reach the scheduler for components
//! mounted late in the hydrate stream — so depth-0
//! `initial_open=true` folders sat under a stuck
//! "loading catalog…" placeholder until the user clicked.
//!
//! Post-fix the per-folder fetch goes through a keyed `Resource`
//! that SSR pre-resolves. The initial HTML arrives populated with
//! every reachable folder's contents inlined in
//! `__RESOLVED_RESOURCES`; the disclosure triangle only toggles
//! native `<details>` visibility against already-loaded data.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn lazy_catalog_tree_has_no_stuck_loading_placeholders() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;

    harness.goto("/").await?;
    harness.wait_for("body", Duration::from_secs(8)).await?;
    // Give hydrate + every Resource a couple of seconds to settle.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Count `<li class="mut">` elements whose visible text contains
    // a "loading…" word in any locale we ship. Post-fix the count
    // should be zero on a populated catalog.
    let stuck: serde_json::Value = harness
        .driver
        .execute_async(
            "const cb=arguments[0];\
             const ls=Array.from(document.querySelectorAll('li.mut, p.mut'));\
             const stuck=ls.filter(x=>{const t=x.textContent.toLowerCase();\
               return t.includes('loading')||t.includes('загруж')||t.includes('lädt')||t.includes('chargement')||t.includes('cargando');});\
             cb({count:stuck.length,texts:stuck.slice(0,5).map(x=>x.textContent.trim())});",
            vec![],
        )
        .await?
        .convert()?;
    let count = stuck
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "catalog tree has {count} stuck `loading catalog…` placeholders: {stuck}"
    );
    harness.close().await
}

#[tokio::test]
async fn clicking_a_folder_shows_its_children_with_no_round_trip() -> Result<()> {
    // Post-fix every folder's children are SSR-pre-resolved; the
    // disclosure click is a pure browser-native operation, no
    // /api/list_dir_page fetch. This guards both correctness AND
    // the performance promise (instant expand).
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    harness.goto("/").await?;
    harness.wait_for(".tree-branch details", Duration::from_secs(10)).await?;

    // Clear the resource-timing log AFTER hydrate so we only count
    // post-mount fetches.
    let _ = harness
        .driver
        .execute_async(
            "const cb=arguments[0];performance.clearResourceTimings();cb(null);",
            vec![],
        )
        .await;

    // Click any folder summary that's currently closed (most depth>0
    // folders mount closed).
    let _ = harness
        .driver
        .execute_async(
            "const cb=arguments[0];\
             for (const d of document.querySelectorAll('.tree-branch details')) {\
               if (!d.open) { d.querySelector('summary').click(); cb('clicked'); return; }\
             }cb('all-open');",
            vec![],
        )
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let fetched: serde_json::Value = harness
        .driver
        .execute_async(
            "const cb=arguments[0];\
             const e=performance.getEntriesByType('resource')\
               .filter(x=>x.name.includes('/api/list_dir_page'));\
             cb({count:e.length});",
            vec![],
        )
        .await?
        .convert()?;
    let count = fetched
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "expand-folder click fired {count} extra /api/list_dir_page request(s) — \
         post-fix the Resource should be SSR-pre-resolved and the click should \
         only toggle native <details> visibility"
    );
    harness.close().await
}
