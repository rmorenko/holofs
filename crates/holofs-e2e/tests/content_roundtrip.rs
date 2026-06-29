//! Batch F: content roundtrip per kind.
//!
//! Most existing tests gauge the system through PNG ingestion. This
//! suite pins the same shape for text/audio/opaque — Content-Type
//! preservation, byte equality where the codec guarantees it, and
//! the kind-specific preview semantics (image LL = thumbnail, audio
//! preview = shorter, text/opaque preview = 404).

use anyhow::Result;
use holofs_e2e::TestHarness;
use reqwest::StatusCode;

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

async fn put_then_get(
    harness: &TestHarness,
    name: &str,
    body: Vec<u8>,
) -> Result<(reqwest::Response, Vec<u8>)> {
    raw_client()
        .put(harness.url(name))
        .body(body)
        .send()
        .await?
        .error_for_status()?;
    let resp = raw_client()
        .get(harness.url(name))
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp
        .bytes()
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    // Re-issue the GET to expose the headers; the first response has
    // already consumed the body. (reqwest::Response is consumed by
    // .bytes(); we need a fresh handle for headers + status.)
    let head = raw_client().get(harness.url(name)).send().await?;
    Ok((head, bytes))
}

// === Text ==================================================================

/// UTF-8 text round-trip: PUT a string with Cyrillic + emoji + ASCII,
/// GET it, the bytes must be exactly preserved and Content-Type
/// stays `text/...`. Catches regressions in the text codec layer
/// where multi-byte sequences could be split across shards.
#[tokio::test]
async fn text_utf8_roundtrip_preserves_multibyte() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("docs").await?;
    let body = "Hello мир! 🌍 holofs ⚡\nSecond line — em-dash + en–dash.\n";
    harness
        .put_bytes("docs/utf8.txt", body.as_bytes().to_vec())
        .await?;
    let resp = raw_client()
        .get(harness.url("docs/utf8.txt"))
        .send()
        .await?
        .error_for_status()?;
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let kind = resp
        .headers()
        .get("x-holofs-kind")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/"), "expected text/* content-type, got {ct:?}");
    assert_eq!(kind, "text", "x-holofs-kind should be 'text', got {kind:?}");
    let bytes = resp.bytes().await?.to_vec();
    let recovered = std::str::from_utf8(&bytes)?;
    assert_eq!(
        recovered, body,
        "text round-trip mangled multibyte characters"
    );
    harness.close().await
}

/// /preview of a text object → 404 (no graceful projection exists).
/// Already partially covered in api_negative but rephrased here so
/// the suite is self-contained per kind.
#[tokio::test]
async fn text_object_has_no_preview() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("docs").await?;
    harness.put_text("docs/np.txt", "no preview").await?;
    let resp = raw_client()
        .get(harness.url("preview/docs/np.txt"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}

// === Audio =================================================================

/// PUT a small WAV, GET it, kind must be `audio`. The decode path
/// goes through symphonia, then re-emits as WAV — round-trip is
/// NOT byte-identical (re-encoded), so the assertion is on
/// reasonable size and the kind label, not exact bytes.
#[tokio::test]
async fn audio_wav_roundtrip_recognised_as_audio_kind() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("audio").await?;
    let wav = holofs_e2e::fixtures::tiny_wav().to_vec();
    let (head, bytes) = put_then_get(&harness, "audio/clip.wav", wav.clone()).await?;
    let kind = head
        .headers()
        .get("x-holofs-kind")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let ct = head
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(kind, "audio", "x-holofs-kind should be 'audio', got {kind:?}");
    assert!(ct.contains("audio"), "audio content-type was {ct:?}");
    // Body should start with the RIFF/WAVE magic — the gateway
    // re-emits a wrapped WAV, even though the inner samples are
    // re-encoded.
    assert!(
        bytes.starts_with(b"RIFF") && bytes.len() > 64,
        "decoded audio doesn't look like a WAV (len={}, first8={:?})",
        bytes.len(),
        &bytes[..bytes.len().min(8)]
    );
    harness.close().await
}

/// /preview/<audio-name> returns a *short* preview WAV — at most
/// equal to the full decoded body, strictly shorter when the audio
/// is long enough that L0 is a proper subset.
#[tokio::test]
async fn audio_preview_is_not_longer_than_full() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("audio").await?;
    let wav = holofs_e2e::fixtures::tiny_wav().to_vec();
    harness.put_bytes("audio/p.wav", wav).await?;
    let full = harness.get_bytes("audio/p.wav").await?;
    let preview = harness.get_bytes("preview/audio/p.wav").await?;
    assert!(
        preview.starts_with(b"RIFF"),
        "audio preview not a WAV: first 8 = {:?}",
        &preview[..preview.len().min(8)]
    );
    assert!(
        preview.len() <= full.len(),
        "audio preview ({}) is longer than full decode ({})",
        preview.len(),
        full.len()
    );
    harness.close().await
}

// === Opaque ================================================================

/// PUT an opaque blob (not image, not audio, not UTF-8), GET it
/// back. Opaque bytes MUST round-trip exactly — there's no codec
/// transform on this path, just RLNC store-and-recall.
#[tokio::test]
async fn opaque_blob_roundtrip_preserves_bytes_exactly() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("files").await?;
    let blob = holofs_e2e::fixtures::tiny_opaque().to_vec();
    harness
        .put_bytes("files/blob.bin", blob.clone())
        .await?;
    let resp = raw_client()
        .get(harness.url("files/blob.bin"))
        .send()
        .await?
        .error_for_status()?;
    let kind = resp
        .headers()
        .get("x-holofs-kind")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let recovered = resp.bytes().await?.to_vec();
    assert_eq!(kind, "opaque", "x-holofs-kind should be 'opaque', got {kind:?}");
    assert_eq!(
        recovered, blob,
        "opaque round-trip changed the bytes (got {}, expected {})",
        recovered.len(),
        blob.len()
    );
    harness.close().await
}

/// Opaque has no preview — same contract as text. Renders the page
/// safe to ask for a preview of any file in the catalog.
#[tokio::test]
async fn opaque_object_has_no_preview() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("files").await?;
    let blob = holofs_e2e::fixtures::tiny_opaque().to_vec();
    harness.put_bytes("files/no-preview.bin", blob).await?;
    let resp = raw_client()
        .get(harness.url("preview/files/no-preview.bin"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    harness.close().await
}

// === Image preview shape ===================================================

/// /preview of an image returns a (smaller) PNG. We don't enforce a
/// strict size ratio — the encoder is non-deterministic — but the
/// preview must be a valid PNG and not exceed the full decode.
#[tokio::test]
async fn image_preview_is_a_smaller_png() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.mkdir_p("photos").await?;
    harness
        .put_bytes(
            "photos/p.png",
            holofs_e2e::fixtures::textured_image_png().to_vec(),
        )
        .await?;
    let full = harness.get_bytes("photos/p.png").await?;
    let prev = harness.get_bytes("preview/photos/p.png").await?;
    let png_magic = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    assert!(
        prev.starts_with(&png_magic),
        "image preview not a PNG: first8 = {:?}",
        &prev[..prev.len().min(8)]
    );
    assert!(
        prev.len() <= full.len(),
        "image preview ({}) is larger than full ({})",
        prev.len(),
        full.len()
    );
    harness.close().await
}
