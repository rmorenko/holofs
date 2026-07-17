//! v2 review round #6 — HTTP export→import round-trip.
//!
//! The `holofs-admin export/import` commands are thin GET/PUT
//! wrappers over the object HTTP surface. This test drives them via
//! their raw HTTP path (`GET /name`, `PUT /name`) to prove:
//!
//! 1. A GET-body from the gateway is a valid input for a subsequent
//!    PUT with `Content-Type: application/octet-stream` — i.e. the
//!    admin backup script's happy path actually round-trips.
//! 2. Delete-then-reimport recovers the object byte-identically.
//!
//! Runs with `--test-threads=1` per the workspace convention.

use anyhow::Result;
use holofs_e2e::TestHarness;

#[tokio::test]
async fn http_export_import_recovers_bytes() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let name = "docs/notes/hello.txt";
    // Use a `&str` (allows non-ASCII literal directly) and take
    // its bytes. Matches how a real export/import would flow —
    // bytes on the wire regardless of what encoding the operator
    // stored.
    let payload_str = "holofs export/import round-trip smoke test\n\
                       line two — with utf-8: файл.txt 🙂\n";
    let payload: &[u8] = payload_str.as_bytes();

    // 1. Seed.
    harness.mkdir_p("docs/notes").await?;
    harness.put_bytes(name, payload.to_vec()).await?;

    // 2. "Export" = plain GET.
    let exported = harness.get_bytes(&format!("/{name}")).await?;
    assert_eq!(
        &exported[..],
        &payload[..],
        "initial GET must match seed bytes"
    );

    // 3. Delete the object.
    harness.delete(name).await?;
    // 4. Verify the delete stuck — GET should now fail. Use `get_bytes`
    //    which returns Err on non-2xx.
    let post_delete = harness.get_bytes(&format!("/{name}")).await;
    assert!(
        post_delete.is_err(),
        "post-delete GET should fail; got Ok({})",
        post_delete.map(|b| b.len()).unwrap_or(0)
    );

    // 5. "Import" = plain PUT of the exported body.
    harness.put_bytes(name, exported.clone()).await?;

    // 6. GET-back must be byte-identical to the original seed.
    let recovered = harness.get_bytes(&format!("/{name}")).await?;
    assert_eq!(
        &recovered[..],
        &payload[..],
        "post-import GET returned different bytes — round-trip lost data"
    );

    harness.close().await
}
