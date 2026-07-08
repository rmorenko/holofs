//! Batch B: API semantics + observability.
//!
//! These tests assert *numeric correctness* and *invariants* on the
//! gateway's observability surface — /api/stats, /api/gc, /metrics —
//! plus the mv operation. The earlier UI tests checked that pages
//! *render*; this suite checks that the numbers underneath are
//! coherent.

use anyhow::{anyhow, Result};
use holofs_e2e::TestHarness;
use reqwest::StatusCode;
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

async fn get_stats(harness: &TestHarness) -> Result<Value> {
    let resp = raw_client()
        .get(harness.url("/api/stats"))
        .send()
        .await?
        .error_for_status()?;
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.contains("application/json"),
        "/api/stats content-type should be application/json, got {ct:?}"
    );
    let body = resp.text().await?;
    Ok(serde_json::from_str(&body)?)
}

async fn run_gc(harness: &TestHarness) -> Result<Value> {
    let body = raw_client()
        .post(harness.url("/api/gc"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&body)?)
}

// === /api/stats ============================================================

/// Fresh gateway with seeding disabled → no objects, zero shards,
/// all counters at zero. Catches regressions where startup-side
/// bookkeeping (cache pre-warm, audit init, etc.) accidentally
/// writes into the counters.
#[tokio::test]
async fn stats_on_empty_gateway_reports_zero_objects() -> Result<()> {
    let mut cfg = holofs_e2e::HarnessConfig::default();
    cfg.extra_env.push(("HOLOFS_NO_SEED".into(), "true".into()));
    let harness = TestHarness::fresh_with(cfg).await?;
    let stats = get_stats(&harness).await?;
    assert_eq!(
        stats["objects_total"].as_u64().ok_or_else(|| anyhow!("missing objects_total"))?,
        0,
        "expected zero objects on a fresh gateway, full stats: {stats}"
    );
    assert_eq!(stats["shards_total"].as_u64().unwrap(), 0);
    assert_eq!(stats["bytes_total"].as_u64().unwrap(), 0);
    assert_eq!(stats["auto_repairs_total"].as_u64().unwrap(), 0);
    assert_eq!(stats["auto_repair_failures_total"].as_u64().unwrap(), 0);
    assert!(
        stats["nodes_total"].as_u64().unwrap() > 0,
        "nodes_total must reflect the configured cluster size"
    );
    harness.close().await
}

/// After N PUTs, `objects_total` must be exactly N (plus directories).
/// Increment-by-one semantics on every successful PUT.
#[tokio::test]
async fn stats_objects_total_increments_per_put() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let base = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    for i in 0..3 {
        harness
            .put_bytes(
                &format!("photos/abstract/n-{i:02}.png"),
                holofs_e2e::fixtures::textured_image_png().to_vec(),
            )
            .await?;
    }
    let after = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    assert_eq!(
        after,
        base + 3,
        "objects_total increment mismatch: before={base} after={after}"
    );
    harness.close().await
}

/// DELETE drops `objects_total` by exactly one. Combined with the
/// per-PUT test above this fixes a strict bookkeeping contract.
#[tokio::test]
async fn stats_objects_total_decrements_on_delete() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/transient.png", body).await?;
    let before = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    assert!(before >= 1);
    harness.delete("photos/transient.png").await?;
    let after = get_stats(&harness).await?["objects_total"].as_u64().unwrap();
    assert_eq!(
        after,
        before - 1,
        "DELETE should drop objects_total by 1: before={before} after={after}"
    );
    harness.close().await
}

// === /metrics ==============================================================

/// /metrics must be valid Prometheus text-format 0.0.4: at minimum
/// a stable Content-Type, a HELP/TYPE line for every exposed gauge,
/// and no NaN values (Prometheus rejects those).
#[tokio::test]
async fn metrics_parses_as_prometheus() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/m.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .get(harness.url("/metrics"))
        .send()
        .await?
        .error_for_status()?;
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await?;
    assert!(
        ct.starts_with("text/plain"),
        "metrics content-type should be text/plain, got {ct:?}"
    );
    assert!(!body.contains("NaN"), "metrics body contains NaN: {body}");

    // Every exposed gauge needs a `# HELP` and `# TYPE` line. Count
    // them; they should match the number of distinct metric names.
    let help_count = body.lines().filter(|l| l.starts_with("# HELP")).count();
    let type_count = body.lines().filter(|l| l.starts_with("# TYPE")).count();
    assert_eq!(
        help_count, type_count,
        "metrics: HELP={help_count} TYPE={type_count} (must match)"
    );
    assert!(
        help_count >= 4,
        "expected at least 4 metrics families exposed, got {help_count}"
    );

    // Sanity-check one well-known name carries a numeric value.
    let nodes_line = body
        .lines()
        .find(|l| l.starts_with("holofs_nodes_total"))
        .ok_or_else(|| anyhow!("missing holofs_nodes_total"))?;
    let val: u64 = nodes_line
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow!("malformed gauge line: {nodes_line}"))?
        .parse()?;
    assert!(val > 0);

    // Async-ingest observability: the three families added with the
    // async 202-Accepted path must be present on every gateway, even
    // when HOLOFS_ASYNC_ENCODE is unset. Fresh gateway → all three
    // must be zero: nothing has started encoding yet.
    for family in [
        "holofs_objects_encoding",
        "holofs_encode_completed_total",
        "holofs_encode_failed_total",
    ] {
        let line = body
            .lines()
            .find(|l| l.starts_with(family) && !l.starts_with('#'))
            .ok_or_else(|| anyhow!("/metrics missing family {family}"))?;
        let n: u64 = line
            .split_whitespace()
            .last()
            .ok_or_else(|| anyhow!("malformed line: {line}"))?
            .parse()?;
        assert_eq!(n, 0, "fresh gateway should report {family}=0, got {n}");
    }
    harness.close().await
}

// === /api/gc ===============================================================

/// GC on a healthy cluster (no orphans) must purge zero shards.
/// Catches regressions where the orphan diff accidentally treats
/// live shards as garbage — that bug would corrupt the catalog.
#[tokio::test]
async fn gc_on_healthy_cluster_purges_nothing() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/g.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    // Snapshot the pre-GC decode so we can prove the harmless GC
    // didn't perturb the bytes (not just "still decodes to >100 b").
    let before = harness.get_bytes("photos/g.png").await?;
    let report = run_gc(&harness).await?;
    let purged = report["purged_total"].as_u64().unwrap_or(u64::MAX);
    assert_eq!(
        purged, 0,
        "healthy GC should purge zero shards, got {purged}; report = {report}"
    );
    let after = harness.get_bytes("photos/g.png").await?;
    assert_eq!(
        after, before,
        "GC on healthy cluster changed the bytes — before={} after={}",
        before.len(),
        after.len()
    );
    harness.close().await
}

/// Running GC twice in a row: the second pass must purge zero shards.
/// Idempotence — there should be no churn between back-to-back GCs.
#[tokio::test]
async fn gc_is_idempotent_back_to_back() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/idem.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let _first = run_gc(&harness).await?;
    let second = run_gc(&harness).await?;
    assert_eq!(
        second["purged_total"].as_u64().unwrap_or(u64::MAX),
        0,
        "second GC should be a no-op, report = {second}"
    );
    harness.close().await
}

/// GC report shape: every node entry must have idx + addr + held +
/// orphaned + ok fields. Drift in this contract would break the
/// `/about` page's GC summary.
#[tokio::test]
async fn gc_report_has_per_node_breakdown() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/bd.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let report = run_gc(&harness).await?;
    let nodes = report["nodes"].as_array().ok_or_else(|| anyhow!("missing nodes array"))?;
    assert!(!nodes.is_empty(), "GC report has empty nodes array");
    for n in nodes {
        assert!(n["idx"].is_number(), "node entry missing idx: {n}");
        assert!(n["addr"].is_string(), "node entry missing addr: {n}");
        assert!(n["held"].is_number(), "node entry missing held: {n}");
        assert!(n["orphaned"].is_number(), "node entry missing orphaned: {n}");
        assert!(n["ok"].is_boolean(), "node entry missing ok: {n}");
    }
    harness.close().await
}

// === /api/mv ===============================================================

/// Basic rename: move an object to a new name in the same directory.
/// The old name must be gone, the new name must decode the same
/// bytes. Cheap smoke that mv doesn't lose payload.
#[tokio::test]
async fn mv_renames_within_directory() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/orig.png", body.clone()).await?;
    let before = harness.get_bytes("photos/orig.png").await?;

    let resp = raw_client()
        .post(harness.url("/api/mv"))
        .form(&[("from", "photos/orig.png"), ("to", "photos/renamed.png")])
        .send()
        .await?;
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 303,
        "mv expected 2xx/303, got {}",
        resp.status()
    );

    // Old name → 404.
    let old = raw_client()
        .get(harness.url("photos/orig.png"))
        .send()
        .await?;
    assert_eq!(old.status(), StatusCode::NOT_FOUND);

    // New name decodes the same bytes.
    let after = harness.get_bytes("photos/renamed.png").await?;
    assert_eq!(after, before, "mv changed the bytes — payload lost");
    harness.close().await
}

/// mv with `to` pointing at an existing entry must fail (no silent
/// clobber). The error contract isn't documented as a single code,
/// so accept any 4xx but reject 5xx / silent success.
#[tokio::test]
async fn mv_to_existing_name_rejects_clobber() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness.put_bytes("photos/a.png", body.clone()).await?;
    harness
        .put_bytes(
            "photos/b.png",
            holofs_e2e::fixtures::tiny_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .post(harness.url("/api/mv"))
        .form(&[("from", "photos/a.png"), ("to", "photos/b.png")])
        .send()
        .await?;
    assert!(
        resp.status().is_client_error(),
        "mv over existing should 4xx, got {}",
        resp.status()
    );
    // Both names must still exist after the failed mv.
    let a = raw_client().get(harness.url("photos/a.png")).send().await?;
    let b = raw_client().get(harness.url("photos/b.png")).send().await?;
    assert_eq!(a.status(), StatusCode::OK, "src vanished after failed mv");
    assert_eq!(b.status(), StatusCode::OK, "dst vanished after failed mv");
    harness.close().await
}

/// mv with missing `from`/`to` form fields → 400. Catches handler-
/// level form parsing regressions.
#[tokio::test]
async fn mv_missing_fields_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let resp = raw_client()
        .post(harness.url("/api/mv"))
        .form(&[("from", "photos/x.png")])
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}
