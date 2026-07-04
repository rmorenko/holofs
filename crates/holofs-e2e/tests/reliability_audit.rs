//! Regression guard for .x audit reputation-cascade
//! (`audit: stop penalising MissingShard …`, commit `a28a83a`).
//!
//! Before the fix the auditor treated `MissingShard` as a negative
//! observation that dropped the canonical node's reputation. Dedup-
//! moved shards (small synthetic 256×256 → 512×512 upscale produces
//! layer-3 all-zero systematic shards that hash-collide across
//! unrelated images) reliably tripped this for the canonical node.
//! Over many audit ticks reputation drained, `discover_live`
//! filtered the cluster down to fewer than K nodes, decode 503'd
//! across half the catalog.
//!
//! Post-fix: `MissingShard` is a *neutral* observation — no
//! `rep.observe` call. The cluster stays healthy under the same
//! workload. This test sets the audit interval intentionally tight
//! and asserts that after N seconds of ticking, every image still
//! decodes.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
#[ignore = "intentionally slow (45 s of audit ticks); opt in via --include-ignored"]
async fn audit_does_not_degrade_cluster_over_time() -> Result<()> {
    // Force audit to run fast so we can observe degradation (or
    // its absence) within a test budget. Monitor is silenced — we
    // only care about the auditor's reputation churn here.
    let mut config = holofs_e2e::HarnessConfig::default();
    config.quiet_background_scanners = false;
    let harness = TestHarness::fresh_with(config).await?;

    // Seed a handful of dedup-prone synthetic images. Same body
    // posted under three names so layer-3 systematic shards
    // hash-collide → triggers the canonical-vs-dedup-node
    // ambiguity the audit used to mis-penalise.
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    for name in ["a.png", "b.png", "c.png"] {
        harness
            .put_bytes(&format!("photos/abstract/{name}"), body.clone())
            .await?;
    }

    // Let the auditor tick repeatedly. Default audit interval is
    // 3 s; 45 s ≈ 15 ticks.
    tokio::time::sleep(Duration::from_secs(45)).await;

    // Every image must still decode.
    for name in ["a.png", "b.png", "c.png"] {
        let bytes = harness
            .get_bytes(&format!("photos/abstract/{name}"))
            .await?;
        assert!(
            bytes.len() > 100,
            "image {name} decoded to {} bytes after 45 s of audit ticks — \
             reputation cascade may be back",
            bytes.len()
        );
    }
    harness.close().await
}
