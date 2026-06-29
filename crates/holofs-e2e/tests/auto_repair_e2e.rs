//! Batch G: auto-repair-on-read end-to-end.
//!
//! Earlier `reliability_repair.rs` pinned the counter SHAPE on
//! /api/stats. This suite proves the counters actually MOVE when
//! the gateway exercises its self-healing path — kill enough nodes
//! to drop a layer below K live shards, GET the object, watch
//! `auto_repairs_total` bump.

use std::time::Duration;

use anyhow::{anyhow, Result};
use holofs_e2e::TestHarness;
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

async fn get_stats(harness: &TestHarness) -> Result<Value> {
    let body = raw_client()
        .get(harness.url("/api/stats"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&body)?)
}

async fn kill_node(harness: &TestHarness, idx: usize) -> Result<()> {
    let resp = raw_client()
        .post(harness.url("/admin/node"))
        .form(&[("i", idx.to_string().as_str())])
        .send()
        .await?;
    let s = resp.status();
    if !(s.is_success() || s.as_u16() == 303) {
        return Err(anyhow!("/admin/node kill {idx} → {s}"));
    }
    Ok(())
}

/// Killing a *small* number of nodes (≤ K worth) leaves every layer
/// above its threshold, so the GET path never enters
/// `decode_with_autorepair`'s retry arm. The counter must stay at 0.
/// Sharper version of `healthy_get_does_not_bump_repair_counters`
/// that also kills a handful of nodes (some shards now unreachable
/// but the layer redundancy absorbs the loss).
#[tokio::test]
async fn light_node_loss_does_not_trigger_repair() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/light.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    // Kill 3 out of 40 (~7%) — well under the lowest-redundancy
    // layer's slack (layer 3 has ~1.15× margin, layer 0 has 4×).
    for i in 0..3 {
        kill_node(&harness, i).await?;
    }
    let _ = harness.get_bytes("photos/light.png").await?;
    let stats = get_stats(&harness).await?;
    let bumped = stats["auto_repairs_total"].as_u64().unwrap_or(u64::MAX);
    assert_eq!(
        bumped, 0,
        "auto-repair should NOT fire under light node loss, got {bumped}; stats = {stats}"
    );
    harness.close().await
}

/// Heavy node loss (≥ 50%) reliably drops at least one layer below
/// K live shards. The GET path then routes through the autorepair
/// retry arm and `auto_repairs_total` increments. We don't pin the
/// exact value (placement is HRW + zone-aware, so the count
/// depends on how many layers were lost), only that it bumped.
#[tokio::test]
async fn heavy_node_loss_triggers_auto_repair_counter() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/heavy.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let total = stats_field(&harness, "nodes_total").await?;
    // Kill 60% of the cluster — enough to push at least the
    // highest-numbered layer below K.
    let to_kill = (total * 6 / 10) as usize;
    for i in 0..to_kill {
        kill_node(&harness, i).await?;
    }
    // GET to drive the decode path through autorepair.
    let _ = raw_client()
        .get(harness.url("photos/heavy.png"))
        .send()
        .await?;
    // Repair fans out RPCs in the background; give it a beat to
    // resolve before reading the counter.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let stats = get_stats(&harness).await?;
    let bumped = stats["auto_repairs_total"].as_u64().unwrap_or(0);
    let failed = stats["auto_repair_failures_total"].as_u64().unwrap_or(0);
    assert!(
        bumped >= 1 || failed >= 1,
        "expected auto_repair counters to move under heavy loss \
         (repairs={bumped}, failures={failed}); full stats = {stats}"
    );
    harness.close().await
}

/// Catastrophic loss: kill all but a handful of nodes. The gateway's
/// repair pass either can't find enough donors or finds the cluster
/// fully degraded — `auto_repair_failures_total` MUST bump, not
/// silently swallow the error.
#[tokio::test]
async fn catastrophic_node_loss_bumps_repair_failures() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/cat.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let total = stats_field(&harness, "nodes_total").await?;
    // Leave only 2 nodes alive — far below K=16. Decode CAN'T
    // succeed even with perfect placement; repair has no donors.
    let to_kill = (total - 2) as usize;
    for i in 0..to_kill {
        kill_node(&harness, i).await?;
    }
    let _ = raw_client()
        .get(harness.url("photos/cat.png"))
        .send()
        .await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let stats = get_stats(&harness).await?;
    let failed = stats["auto_repair_failures_total"].as_u64().unwrap_or(0);
    assert!(
        failed >= 1,
        "catastrophic node loss should bump auto_repair_failures_total, \
         got 0; full stats = {stats}"
    );
    harness.close().await
}

async fn stats_field(harness: &TestHarness, field: &str) -> Result<u64> {
    let s = get_stats(harness).await?;
    s.get(field)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing {field} in /api/stats: {s}"))
}
