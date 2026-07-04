//! `/escrow` split + recover.
//!
//! The page hosts a multipart upload form for splitting an
//! arbitrary file into N shares with threshold K, and a second
//! form for recovering from any K of N shares. Shares are NOT
//! stored in the cluster — they're a side-channel artifact.

use std::time::Duration;

use anyhow::Result;
use holofs_e2e::TestHarness;
use thirtyfour::prelude::*;

#[tokio::test]
async fn escrow_index_page_renders_two_forms() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    harness.goto("/escrow").await?;
    harness.wait_for("body", Duration::from_secs(8)).await?;
    let forms = harness.driver.find_all(By::Css("form")).await?;
    assert!(
        forms.len() >= 2,
        "/escrow expected at least two <form> elements (split + recover), got {}",
        forms.len()
    );
    let body = harness.driver.find(By::Css("body")).await?.text().await?;
    let lc = body.to_lowercase();
    assert!(
        lc.contains("split") || lc.contains("разд") || lc.contains("teil") || lc.contains("dividir") || lc.contains("découper"),
        "/escrow body does not mention 'split' / 'recover' in any locale: {body:.300}"
    );
    harness.close().await
}

#[tokio::test]
async fn split_then_recover_roundtrips_a_secret() -> Result<()> {
    let harness = TestHarness::fresh().await?;
    let secret = b"holofs e2e escrow secret payload\n";

    // 1. POST /escrow/split with k=2 n=3.
    let split_form = reqwest::multipart::Form::new()
        .text("k", "2")
        .text("n", "3")
        .part(
            "file",
            reqwest::multipart::Part::bytes(secret.to_vec())
                .file_name("secret.txt")
                .mime_str("text/plain")?,
        );
    let split_html = reqwest::Client::new()
        .post(harness.url("/escrow/split"))
        .multipart(split_form)
        .send()
        .await?
        .text()
        .await?;

    // The split-result page emits links to each share's download
    // endpoint: /escrow/download/<eid>_<idx>.holoshare. Pull two.
    let mut share_links: Vec<String> = Vec::new();
    for line in split_html.split(|c| c == '"' || c == '\n') {
        if line.contains("/escrow/download/") && line.ends_with(".holoshare") {
            let candidate = line.trim_matches(|c: char| !c.is_ascii_graphic());
            if !share_links.contains(&candidate.to_string()) {
                share_links.push(candidate.to_string());
            }
            if share_links.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(
        share_links.len(),
        2,
        "could not find 2 distinct /escrow/download/ links in split HTML: {} bytes",
        split_html.len()
    );

    // 2. Fetch share bytes.
    let mut shares: Vec<Vec<u8>> = Vec::new();
    for href in &share_links {
        // hrefs may be path-only (`/escrow/download/...`) or absolute.
        let url = if href.starts_with("http") {
            href.clone()
        } else {
            harness.url(href)
        };
        let bytes = reqwest::Client::new()
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        shares.push(bytes.to_vec());
    }

    // 3. POST /escrow/recover with the 2 shares.
    let recover_form = reqwest::multipart::Form::new()
        .part(
            "shares",
            reqwest::multipart::Part::bytes(shares[0].clone())
                .file_name("share_0.holoshare")
                .mime_str("application/octet-stream")?,
        )
        .part(
            "shares",
            reqwest::multipart::Part::bytes(shares[1].clone())
                .file_name("share_1.holoshare")
                .mime_str("application/octet-stream")?,
        );
    let recovered = reqwest::Client::new()
        .post(harness.url("/escrow/recover"))
        .multipart(recover_form)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(
        recovered.as_ref(),
        secret,
        "k=2 of n=3 recovery should round-trip the original bytes; got {} bytes",
        recovered.len()
    );
    harness.close().await
}
