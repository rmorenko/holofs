//! Concurrency tests: parallel PUT / GET / DELETE.
//!
//! These tests stress the gateway's per-object locking and the
//! `purge_orphans_of` path. None of them rely on UI
//! rendering; they all hit the gateway's HTTP API directly to keep
//! the wall-clock cost low.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;

/// Spawn N concurrent PUTs to N distinct names, await them all,
/// then verify every body decodes byte-perfectly.
#[tokio::test]
async fn parallel_puts_to_distinct_names_all_succeed() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let names: Vec<String> = (0..8)
        .map(|i| format!("photos/abstract/parallel-{i:02}.png"))
        .collect();
    // Use bare reqwest to avoid the per-helper borrow on
    // `harness`; we only need the parallel PUT semantics, not
    // the JSON-decoded response.
    let client = reqwest::Client::new();
    let mut futures = Vec::new();
    for name in &names {
        let body = body.clone();
        let url = harness.url(name);
        let client = client.clone();
        futures.push(async move {
            client.put(url).body(body).send().await
        });
    }
    let responses = futures::future::join_all(futures).await;
    for r in responses {
        let resp = r?;
        assert!(
            resp.status().is_success(),
            "parallel PUT got HTTP {}",
            resp.status()
        );
    }
    // Every uploaded object must decode AND — since the 8 names all
    // received the same fixture bytes — must decode to the exact same
    // byte string. A weaker `len() > 100` check used to let a partial-
    // corruption regression slip (different sizes, all > 100). The
    // first survivor sets the baseline; the rest must match it.
    let mut baseline: Option<Vec<u8>> = None;
    for name in &names {
        let resp = reqwest::Client::new().get(harness.url(name)).send().await?;
        assert!(resp.status().is_success());
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("image/"),
            "parallel PUT survivor {name} returned content-type {ct:?}"
        );
        let bytes = resp.bytes().await?.to_vec();
        match &baseline {
            None => baseline = Some(bytes),
            Some(b0) => assert_eq!(
                &bytes, b0,
                "parallel PUT survivors decoded to *different* bytes — \
                 expected identical decode since all 8 PUTs sent the \
                 same fixture (got {} vs baseline {})",
                bytes.len(),
                b0.len()
            ),
        }
    }
    harness.close().await
}

/// While one PUT is in flight, a GET on the SAME name either
/// returns the old bytes or the new bytes — never garbage and
/// never a 5xx. Guards the catalog mutex + put_orphans purge
/// interleaving.
#[tokio::test]
async fn get_during_put_replace_never_returns_garbage() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;
    let body_a = holofs_e2e::fixtures::textured_image_png().to_vec();
    let body_b = holofs_e2e::fixtures::tiny_image_png().to_vec();
    let name = "photos/abstract/race.png";
    harness.put_bytes(name, body_a.clone()).await?;
    // Capture the two valid-bytes values so we can match against
    // either one without false positives.
    let valid_a = harness.get_bytes(name).await?;
    harness.put_bytes(name, body_b.clone()).await?;
    let valid_b = harness.get_bytes(name).await?;
    assert_ne!(valid_a, valid_b, "test fixtures collided after re-PUT");

    // Now race a flood of PUT-replaces vs GETs.
    let put_task = {
        let body_a = body_a.clone();
        let body_b = body_b.clone();
        let name = name.to_string();
        let base = harness.base_url.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            for i in 0..6 {
                let body = if i % 2 == 0 { body_a.clone() } else { body_b.clone() };
                let _ = client
                    .put(format!("{base}/{name}"))
                    .body(body)
                    .send()
                    .await;
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        })
    };

    let mut get_failures: Vec<String> = Vec::new();
    let client = reqwest::Client::new();
    for _ in 0..20 {
        let resp = client.get(harness.url(name)).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let bytes = r.bytes().await.unwrap_or_default().to_vec();
                if bytes != valid_a && bytes != valid_b {
                    get_failures.push(format!(
                        "GET returned {} bytes — neither valid_a nor valid_b",
                        bytes.len()
                    ));
                }
            }
            Ok(r) => get_failures.push(format!("GET status = {}", r.status())),
            Err(e) => get_failures.push(format!("GET error = {e}")),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    put_task.await?;
    assert!(
        get_failures.is_empty(),
        "concurrent GET vs PUT-replace produced {} bad responses: {:?}",
        get_failures.len(),
        &get_failures[..get_failures.len().min(5)]
    );
    harness.close().await
}

/// GC running while a PUT is in flight must not silently swallow
/// the new shards. The gateway's `gc_barrier` enforces a writer/GC
/// rendezvous; this test asserts the new object decodes after the
/// race.
#[tokio::test]
async fn gc_during_put_does_not_eat_fresh_shards() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos/abstract").await?;

    // Pre-seed something to keep GC honest.
    harness
        .put_bytes(
            "photos/abstract/baseline.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;

    // Capture a reference decode of the same body, BEFORE the race,
    // by writing it to a separate name. Whatever the race-with-gc
    // PUT decodes to later must equal this baseline byte-for-byte —
    // a weaker `len() > 100` check used to let off-by-one shard
    // corruption slip past.
    let race_body = holofs_e2e::fixtures::tiny_image_png().to_vec();
    harness
        .put_bytes("photos/abstract/baseline-tiny.png", race_body.clone())
        .await?;
    let baseline_decode = harness
        .get_bytes("photos/abstract/baseline-tiny.png")
        .await?;

    let put_task = {
        let base = harness.base_url.clone();
        let body = race_body.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            // Tiny delay so GC has a chance to fire first.
            tokio::time::sleep(Duration::from_millis(80)).await;
            client
                .put(format!("{base}/photos/abstract/race-with-gc.png"))
                .body(body)
                .send()
                .await
        })
    };
    let gc_task = {
        let base = harness.base_url.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            client.post(format!("{base}/api/gc")).send().await
        })
    };
    let (put_res, gc_res) = tokio::join!(put_task, gc_task);
    let put_resp = put_res?;
    let gc_resp = gc_res?;
    assert!(
        put_resp.is_ok() && put_resp.unwrap().status().is_success(),
        "concurrent PUT failed"
    );
    assert!(
        gc_resp.is_ok() && gc_resp.unwrap().status().is_success(),
        "concurrent GC failed"
    );
    // The freshly-PUT shards must survive AND decode to the same
    // bytes the baseline did. A length-only check used to pass
    // even if GC silently shaved a layer off the new manifest —
    // the decode still produced a "PNG-ish" blob, just at a lower
    // quality / different size.
    let bytes = harness
        .get_bytes("photos/abstract/race-with-gc.png")
        .await?;
    assert_eq!(
        bytes, baseline_decode,
        "post-race decode bytes differ from baseline-tiny — GC may have \
         eaten shards from the freshly-PUT object (post-race={}, baseline={})",
        bytes.len(),
        baseline_decode.len()
    );
    harness.close().await
}
