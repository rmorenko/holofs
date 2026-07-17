//! P2.2 — /admin/retention set/get end-to-end.
//!
//! Covers the HTTP surface the CLI + operator dashboards will use.
//! The GC daemon itself runs on the gateway; verifying it deletes
//! on tick would need the harness to seed an already-expired object
//! + wait for a tick, which is heavier than the schema/API smoke
//! this file provides. Unit coverage for the GC policy lives in
//! `crates/holofs-gateway/tests/directory_ops.rs`.
//!
//! Runs with `--test-threads=1` per the workspace convention.

use anyhow::{anyhow, Result};
use holofs_e2e::TestHarness;

#[tokio::test]
async fn retention_roundtrip_via_admin_endpoints() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;
    let name = "docs/notes/hello.txt";

    // 1. Baseline: fresh object has no policy.
    let get_url = format!("/admin/retention/{name}");
    let v = harness.get_json(&get_url).await?;
    assert_eq!(
        v.get("policy"),
        Some(&serde_json::Value::Null),
        "fresh object must report policy=null"
    );

    // 2. Set an expiry.
    let post = harness
        .post_form(
            "/admin/retention",
            &[("name", name), ("expires_at_unix", "1700000000")],
        )
        .await?;
    let policy = post
        .get("policy")
        .ok_or_else(|| anyhow!("POST response missing `policy`"))?;
    let got_expires = policy
        .get("expires_at_unix")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("policy missing expires_at_unix: {policy:?}"))?;
    assert_eq!(got_expires, 1_700_000_000);

    // 3. Read back — the policy must persist.
    let v = harness.get_json(&get_url).await?;
    let got = v
        .get("policy")
        .and_then(|p| p.get("expires_at_unix"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("GET after set missing policy"))?;
    assert_eq!(got, 1_700_000_000);

    // 4. Clear.
    let post = harness
        .post_form("/admin/retention", &[("name", name), ("clear", "true")])
        .await?;
    assert_eq!(post.get("policy"), Some(&serde_json::Value::Null));

    // 5. And the GET agrees.
    let v = harness.get_json(&get_url).await?;
    assert_eq!(v.get("policy"), Some(&serde_json::Value::Null));

    harness.close().await
}
