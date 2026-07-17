//! v2 review round #2a — real drain-node migration end-to-end.
//!
//! The unit tests in `crates/holofs-cluster/tests` cover only the
//! degenerate branches of `drain_node_from_manifest` (empty catalog,
//! directory-only, drain_idx out-of-range). This e2e exercises the
//! actual donor-fetch → RLNC re-emit → PUT+CAS network flow inside
//! `drain_node_from_manifest`, driven from the operator's HTTP surface
//! (`POST /admin/drain_node`) so the whole path — HTTP handler →
//! Gateway::drain_node → cluster::rebalance::drain_node → per-object
//! rebalance — is asserted end-to-end.
//!
//! Runs with `--test-threads=1` per the workspace convention.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn drain_node_flow_preserves_object_readability() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.seed_minimal().await?;

    // Baseline GET: the seeded text object round-trips. Store its
    // bytes; after drain those bytes must be reproducible.
    let baseline = harness.get_bytes("/docs/notes/hello.txt").await?;
    assert!(
        !baseline.is_empty(),
        "seeded text object should have non-empty body"
    );

    // Ask /admin/drain_node to drain a middle-index node. The
    // harness cluster ships 40 embedded nodes (see `holofs-core::
    // N_NODES`); idx=5 is safely inside range and not one of the
    // gateway's canonical zero-index picks. We don't pass `--purge`
    // — we want migration only, not filesystem wipe.
    let resp = harness
        .post_form("/admin/drain_node", &[("idx", "5"), ("purge", "false")])
        .await?;

    // Response schema: `{idx, admin_kill_set, drained_objects,
    // failed_objects, purged, warning}`. Every field is asserted so
    // a schema regression in the handler doesn't silently pass.
    for f in [
        "idx",
        "admin_kill_set",
        "drained_objects",
        "failed_objects",
        "purged",
        "warning",
    ] {
        assert!(
            resp.get(f).is_some(),
            "drain_node response missing `{f}`: {resp:?}"
        );
    }
    assert_eq!(resp.get("idx").and_then(|v| v.as_u64()), Some(5));
    let failed = resp
        .get("failed_objects")
        .and_then(|v| v.as_u64())
        .expect("failed_objects must be numeric");
    assert_eq!(
        failed, 0,
        "drain sweep failed on {failed} objects — real network migration \
         path returned errors"
    );

    // Post-drain: the same object must still be readable — that's
    // the whole point of drain-with-migration. If HRW re-emitted
    // shards onto the shrunk live set and the catalog was updated
    // via CAS, the GET path fetches from the new placements.
    // Small delay lets the catalog persist path flush.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = harness.get_bytes("/docs/notes/hello.txt").await?;
    assert_eq!(
        after, baseline,
        "post-drain GET returned different bytes — migration lost data"
    );

    harness.close().await
}
