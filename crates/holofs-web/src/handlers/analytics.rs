//! Wavelet-mix + spotlight + fingerprint axum handlers.
//!
//! - `GET /api/fingerprint/*path` — [`api_fingerprint`] JSON
//!   perceptual-hash lookup.
//! - `GET /api/mix.png?a=&b=&split=` — [`mix_preview`] Stage 12.6
//!   wavelet-mix hybrid PNG (streamed as the `<img src>` on the
//!   `/mix` page).
//! - `POST /api/mix-save` — [`mix_save`] build the hybrid and
//!   ingest it under `dest`, redirecting to the catalog page with
//!   the destination's parent opened.
//! - `GET /api/spotlight.png?name=&x=&y=&w=&h=&mode=` —
//!   [`spotlight_png`] Stage 13.2 (spatial) / Stage 14.1 (coeff)
//!   composite render.
//!
//! Moved out of `handlers.rs` in Phase R2b.5.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use holofs_gateway::{Gateway, SpotlightRoi};

use super::response::fingerprint_to_json;
use super::util::{
    bad_request, error_to_response, is_valid_put_name, json_response, parse_urlencoded_field,
    redirect_to, url_encode_simple,
};

/// `GET /api/fingerprint/*path` — JSON `{name, fingerprint, kind}`.
pub async fn api_fingerprint(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    match gw.fingerprint_of(&name).await {
        Ok(info) => json_response(StatusCode::OK, fingerprint_to_json(&info)),
        Err(e) => error_to_response(e),
    }
}

/// Stage 12.6: `GET /api/mix.png?a=&b=&split=` — stream the
/// wavelet-mix hybrid PNG. Used by the `/mix` page as the preview
/// `<img src>`; the gateway does the actual decode + IDWT + PNG
/// encode. Returns `400` with a plain-text body on parameter or
/// compatibility errors so the preview can render an inline
/// message.
pub async fn mix_preview(
    RawQuery(raw_query): RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let qs = raw_query.unwrap_or_default();
    let a = parse_urlencoded_field(&qs, "a").unwrap_or_default();
    let b = parse_urlencoded_field(&qs, "b").unwrap_or_default();
    let split: u8 = parse_urlencoded_field(&qs, "split")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if a.is_empty() {
        return bad_request("missing 'a'");
    }
    if b.is_empty() {
        return bad_request("missing 'b'");
    }
    match gw.mix_objects(&a, &b, split).await {
        Ok(out) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            out.bytes,
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// Stage 12.6: `POST /api/mix-save` (form: a, b, split, dest) —
/// builds the hybrid and ingests it into the catalog at `dest`,
/// then 303-redirects to the catalog with `?open=<parent>` so the
/// new entry is visible.
pub async fn mix_save(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(a) = parse_urlencoded_field(body_str, "a") else {
        return bad_request("missing 'a'");
    };
    let Some(b) = parse_urlencoded_field(body_str, "b") else {
        return bad_request("missing 'b'");
    };
    let split: u8 = match parse_urlencoded_field(body_str, "split")
        .and_then(|s| s.parse().ok())
    {
        Some(n) => n,
        None => return bad_request("missing or invalid 'split'"),
    };
    let Some(dest) = parse_urlencoded_field(body_str, "dest") else {
        return bad_request("missing 'dest'");
    };
    if !is_valid_put_name(&dest) {
        return bad_request("invalid 'dest' (reserved or empty top segment)");
    }
    let out = match gw.mix_objects(&a, &b, split).await {
        Ok(o) => o,
        Err(e) => return error_to_response(e),
    };
    if let Err(e) = gw.ingest_bytes(&dest, &out.bytes).await {
        return error_to_response(e);
    }
    gw.embed_object_in_background(dest.clone());
    // Land the user on the tree view with the destination's parent
    // expanded — same pattern as mkdir form.
    let parent = dest.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = if parent.is_empty() {
        format!("/?open={}", url_encode_simple(&dest))
    } else {
        format!("/?open={}", url_encode_simple(&parent))
    };
    redirect_to(&target)
}

/// Stage 13.2: `GET /api/spotlight.png?name=...&x=N&y=N&w=N&h=N` —
/// composited PNG (L0 blur outside the ROI, full resolution inside).
/// All ROI params are floats in `[0, 1]` normalised against the
/// image width / height. `?mode=coeff` selects the Stage 14.1
/// Haar-coefficient-mask variant; default (`spatial` / anything
/// else) keeps the Stage 13.2 two-pass spatial composite.
pub async fn spotlight_png(
    RawQuery(raw): RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let Some(name) = raw.as_deref().and_then(|s| parse_urlencoded_field(s, "name")) else {
        return bad_request("missing 'name'");
    };
    let parse = |k: &str, dflt: f32| -> f32 {
        raw.as_deref()
            .and_then(|s| parse_urlencoded_field(s, k))
            .and_then(|s| s.parse().ok())
            .unwrap_or(dflt)
    };
    let roi = SpotlightRoi {
        x: parse("x", 0.35),
        y: parse("y", 0.35),
        w: parse("w", 0.3),
        h: parse("h", 0.3),
    };
    let mode = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "mode"))
        .unwrap_or_else(|| "spatial".to_string());
    let result = if mode == "coeff" {
        gw.spotlight_coeff(&name, roi).await
    } else {
        gw.spotlight(&name, roi).await
    };
    match result {
        Ok(out) => {
            let mut resp = Response::new(axum::body::Body::from(out.bytes));
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            h.insert(
                HeaderName::from_static("x-holofs-roi-px"),
                HeaderValue::from_str(&format!(
                    "{},{},{},{}",
                    out.roi_px.0, out.roi_px.1, out.roi_px.2, out.roi_px.3
                ))
                .unwrap_or(HeaderValue::from_static("0,0,0,0")),
            );
            h.insert(
                HeaderName::from_static("x-holofs-decode-ms"),
                HeaderValue::from_str(&out.decode_ms.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            h.insert(
                HeaderName::from_static("x-holofs-bytes-downloaded"),
                HeaderValue::from_str(&out.bytes_downloaded.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            resp
        }
        Err(e) => error_to_response(e),
    }
}
