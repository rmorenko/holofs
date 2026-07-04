//! Regression guard for (`gateway: DELETE / PUT-replace must
//! not purge shards shared with other objects`, commit `804da77`).
//!
//! Two objects with byte-identical content share a `data_cid` and
//! therefore an `object_id`. Before the fix, deleting one of them
//! issued `Request::Purge { object_id }` to every node, which removed
//! the entire `(object_id, *, *)` bucket — and the SURVIVING object's
//! GET started returning 503 with `image decode failed`.
//!
//! The fix routes both DELETE and the versions-off PUT-replace through
//! `purge_orphans_of`, which walks the rest of the catalog +
//! version-archive directory and only ships hashes-nobody-else-keeps
//! to `PurgeByHash`. This test puts the same bytes under two names,
//! deletes one, then re-fetches the other byte-perfectly.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn delete_of_dedup_sibling_does_not_break_survivor() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;

    // Two PUTs with the EXACT same body → same data_cid → same
    // object_id → shared bucket on every node via PUT-time dedup.
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    harness
        .put_bytes("photos/abstract/a.png", body.clone())
        .await?;
    harness.put_bytes("photos/abstract/b.png", body).await?;

    // Capture survivor's pre-delete bytes so we can compare byte-
    // perfectly after the dedup-sibling DELETE.
    let before = harness.get_bytes("photos/abstract/b.png").await?;

    // DELETE the sibling. The buggy code path would purge the shared
    // bucket on every node here.
    harness.delete("photos/abstract/a.png").await?;
    // Give a moment for the purge fan-out + any background tasks.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The survivor must still decode and return the same bytes.
    let after = harness.get_bytes("photos/abstract/b.png").await?;
    assert_eq!(
        before, after,
        "survivor bytes changed after dedup-sibling DELETE — \
         `purge_orphans_of` is over-purging again"
    );
    harness.close().await
}

#[tokio::test]
async fn put_replace_of_dedup_sibling_does_not_break_survivor() -> Result<()> {
    // Same guard but the trigger is a PUT-replace, not a DELETE.
    // versions=true on the test harness archives the prior manifest
    // (no purge runs), but versions-off goes through `purge_orphans_of`
    // — that's the path we want to exercise.
    let mut config = holofs_e2e::HarnessConfig::default();
    config.enable_versions = false;
    let harness = TestHarness::fresh_with(config).await?;
    harness.mkdir_p("photos/abstract").await?;

    let body_a = holofs_e2e::fixtures::textured_image_png().to_vec();
    let body_b = holofs_e2e::fixtures::tiny_image_png().to_vec();
    harness
        .put_bytes("photos/abstract/a.png", body_a.clone())
        .await?;
    harness
        .put_bytes("photos/abstract/b.png", body_a)
        .await?;

    let before = harness.get_bytes("photos/abstract/b.png").await?;
    // PUT-replace `a.png` with DIFFERENT bytes — the new write has
    // a different data_cid, so the old (shared with b.png) bucket
    // becomes orphan-of-a's-prior-content. `purge_orphans_of` must
    // realise that hash set is still held by `b.png` and skip the
    // purge.
    harness.put_bytes("photos/abstract/a.png", body_b).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = harness.get_bytes("photos/abstract/b.png").await?;
    assert_eq!(
        before, after,
        "survivor bytes changed after PUT-replace of dedup sibling — \
         `purge_orphans_of` (versions-off branch) is over-purging"
    );
    harness.close().await
}
