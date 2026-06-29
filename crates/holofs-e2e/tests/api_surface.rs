//! Batch D: undertested API surfaces.
//!
//! - `/escrow/split` + `/escrow/download/<share>` + `/escrow/recover`
//!   — full byte round-trip with k-of-n share reassembly.
//! - `/api/shard/<c_l_idx>/<name>` — per-shard PNG inspector.
//! - `/preview/stream/<name>` — multipart progressive-layer stream.
//! - `/api/fingerprint/<name>` — perceptual fingerprint JSON.
//! - `/api/upload` — multipart-form upload path.

use anyhow::{anyhow, Result};
use holofs_e2e::TestHarness;
use reqwest::{multipart, StatusCode};
use serde_json::Value;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

// === /escrow round-trip ===================================================

/// Split a file into k-of-n shares, download k of them, recover the
/// original bytes via /escrow/recover. The classic Shamir-style
/// integration test — exercises every escrow handler in one shot.
#[tokio::test]
async fn escrow_split_then_recover_round_trips_bytes() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    // Use a non-trivial body so the split/reassemble proves itself
    // on something larger than k bytes.
    let body: Vec<u8> = (0..4096).map(|i| (i as u8).wrapping_mul(7)).collect();

    let split_form = multipart::Form::new()
        .part(
            "file",
            multipart::Part::bytes(body.clone())
                .file_name("secret.bin")
                .mime_str("application/octet-stream")?,
        )
        .text("k", "3")
        .text("n", "5")
        .text("lang", "en");

    let split_resp = raw_client()
        .post(harness.url("/escrow/split"))
        .multipart(split_form)
        .send()
        .await?
        .error_for_status()?;
    assert!(
        split_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/html"),
        "split response must be HTML"
    );
    let split_html = split_resp.text().await?;
    // The HTML lists each share's download link as href="/escrow/download/...".
    // We pull at least the first 3 paths out of the markup.
    let share_paths: Vec<String> = split_html
        .match_indices("/escrow/download/")
        .filter_map(|(start, _)| {
            let tail = &split_html[start..];
            let end_q = tail.find('"').unwrap_or(tail.len());
            Some(tail[..end_q].to_string())
        })
        .collect();
    assert!(
        share_paths.len() >= 3,
        "expected ≥3 share download links in split HTML, found {} (preview: {})",
        share_paths.len(),
        &split_html[split_html.len().saturating_sub(400)..]
    );

    // Download exactly k=3 shares.
    let mut share_blobs: Vec<Vec<u8>> = Vec::new();
    for path in share_paths.iter().take(3) {
        let bytes = raw_client()
            .get(harness.url(path))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        assert!(!bytes.is_empty(), "share download was empty for {path}");
        share_blobs.push(bytes);
    }

    // Recover via multipart POST with k `shares` fields.
    let mut recover_form = multipart::Form::new();
    for (i, blob) in share_blobs.into_iter().enumerate() {
        recover_form = recover_form.part(
            "shares",
            multipart::Part::bytes(blob)
                .file_name(format!("share_{i:02}.holoshare"))
                .mime_str("application/octet-stream")?,
        );
    }
    let recover_resp = raw_client()
        .post(harness.url("/escrow/recover"))
        .multipart(recover_form)
        .send()
        .await?
        .error_for_status()?;
    let recovered = recover_resp.bytes().await?.to_vec();
    assert_eq!(
        recovered, body,
        "/escrow/recover did NOT round-trip — got {} bytes vs original {}",
        recovered.len(),
        body.len()
    );
    harness.close().await
}

/// /escrow/recover with too few shares (below k) must fail cleanly.
#[tokio::test]
async fn escrow_recover_with_too_few_shares_rejects() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let body: Vec<u8> = (0..1024).map(|i| (i * 13) as u8).collect();
    let split_form = multipart::Form::new()
        .part(
            "file",
            multipart::Part::bytes(body)
                .file_name("s.bin")
                .mime_str("application/octet-stream")?,
        )
        .text("k", "3")
        .text("n", "5")
        .text("lang", "en");
    let split_html = raw_client()
        .post(harness.url("/escrow/split"))
        .multipart(split_form)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let share_paths: Vec<String> = split_html
        .match_indices("/escrow/download/")
        .filter_map(|(start, _)| {
            let tail = &split_html[start..];
            let end_q = tail.find('"').unwrap_or(tail.len());
            Some(tail[..end_q].to_string())
        })
        .collect();
    // Only feed back 2 shares (k=3 required).
    let mut recover_form = multipart::Form::new();
    for (i, path) in share_paths.iter().take(2).enumerate() {
        let blob = raw_client()
            .get(harness.url(path))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        recover_form = recover_form.part(
            "shares",
            multipart::Part::bytes(blob)
                .file_name(format!("share_{i:02}.holoshare"))
                .mime_str("application/octet-stream")?,
        );
    }
    let resp = raw_client()
        .post(harness.url("/escrow/recover"))
        .multipart(recover_form)
        .send()
        .await?;
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "below-threshold recover should fail, got {}",
        resp.status()
    );
    harness.close().await
}

// === /api/shard ===========================================================

/// /api/shard/<c_l_idx>/<path> returns a grayscale PNG for one
/// shard's payload. The handler's `c_l_idx` parsing is fiddly
/// (must be `<c>_<l>_<idx>`), so this test also pins the BadRequest
/// branch.
#[tokio::test]
async fn api_shard_returns_png_for_valid_triple() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/inspect.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .get(harness.url("/api/shard/0_0_0/photos/inspect.png"))
        .send()
        .await?;
    let status = resp.status();
    assert_eq!(
        status,
        StatusCode::OK,
        "/api/shard expected 200, got {status}"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.contains("image"), "shard content-type was {ct:?}");
    let bytes = resp.bytes().await?.to_vec();
    assert!(is_png(&bytes), "shard payload is not PNG: first 8 = {:?}", &bytes[..bytes.len().min(8)]);
    harness.close().await
}

/// /api/shard with a malformed `c_l_idx` (not three '_'-separated
/// integers) → 400. Catches handler-level parser regressions.
#[tokio::test]
async fn api_shard_with_malformed_triple_returns_400() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/inspect.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .get(harness.url("/api/shard/not-a-triple/photos/inspect.png"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    harness.close().await
}

// === /preview/stream ======================================================

/// /preview/stream returns multipart/x-mixed-replace progressively
/// revealing layers. We just assert the response opens with the
/// expected boundary header and the body contains at least two
/// boundary markers (i.e. at least two frames were sent before EOF).
#[tokio::test]
async fn preview_stream_emits_multipart_frames() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/holo.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .get(harness.url("/preview/stream/photos/holo.png"))
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
        ct.contains("multipart/x-mixed-replace"),
        "expected multipart/x-mixed-replace, got {ct:?}"
    );
    // Pull a chunk of the body and look for boundary markers. The
    // boundary string is encoded in the content-type header after
    // `boundary=`.
    let boundary = ct
        .split("boundary=")
        .nth(1)
        .ok_or_else(|| anyhow!("no boundary in content-type {ct:?}"))?
        .trim_matches('"')
        .to_string();
    let body = resp.bytes().await?.to_vec();
    let marker = format!("--{boundary}");
    let count = body
        .windows(marker.len())
        .filter(|w| *w == marker.as_bytes())
        .count();
    assert!(
        count >= 2,
        "expected ≥2 boundary markers in stream body, found {count} (body len = {})",
        body.len()
    );
    harness.close().await
}

// === /api/fingerprint ====================================================

/// /api/fingerprint/<path> returns a JSON blob with `fingerprint`
/// (hex) and `kind`. Pins the schema the /similar UI consumes.
#[tokio::test]
async fn api_fingerprint_returns_hex_for_image() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/fp.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let resp = raw_client()
        .get(harness.url("/api/fingerprint/photos/fp.png"))
        .send()
        .await?
        .error_for_status()?;
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body)?;
    let fp = v["fingerprint"].as_str().ok_or_else(|| anyhow!("no fingerprint field: {body}"))?;
    assert!(
        fp.len() >= 8 && fp.chars().all(|c| c.is_ascii_hexdigit()),
        "fingerprint not lowercase hex: {fp:?}"
    );
    assert_eq!(v["kind"].as_str(), Some("image"));
    harness.close().await
}

// === /api/upload ==========================================================

/// /api/upload multipart with file + parent → the file lands at
/// `parent/<filename>` in the catalog. Mirror of the catalog page's
/// drag-and-drop form.
#[tokio::test]
async fn api_upload_multipart_lands_in_target_dir() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("uploads").await?;
    let body = holofs_e2e::fixtures::textured_image_png().to_vec();
    let form = multipart::Form::new()
        .text("parent", "uploads")
        .part(
            "file",
            multipart::Part::bytes(body.clone())
                .file_name("dropped.png")
                .mime_str("image/png")?,
        );
    let resp = raw_client()
        .post(harness.url("/api/upload"))
        .multipart(form)
        .send()
        .await?;
    let s = resp.status();
    assert!(
        s.is_success() || s.as_u16() == 303,
        "/api/upload expected 2xx/303, got {s}"
    );
    // The file must now be GET-able under uploads/dropped.png.
    let bytes = harness.get_bytes("uploads/dropped.png").await?;
    assert!(
        is_png(&bytes),
        "uploaded file is not a PNG on the read side"
    );
    harness.close().await
}
