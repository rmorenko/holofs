//! CLIP-based semantic search axum handlers.
//!
//! - `POST /api/embed_all` — [`embed_all`]. Kicks off a one-shot bulk
//!   embed of every image in the catalog that isn't in
//!   `embeddings.bin` yet. Synchronous — the request hangs until the
//!   walk finishes — because the typical run on a few hundred images
//!   is sub-minute and the curl caller wants the final `(new,
//!   skipped)` counts to print.
//! - `GET /api/search?q=...&limit=...&band=...` — [`semantic_search`].
//!   Natural-language cosine-similarity query against the CLIP
//!   embedding index. Returns JSON list of `{name, score, band}`
//!   sorted by cosine descending.
//!
//! Both routes 503 with a diagnostic body when the embedding feature
//! is off (`--enable-embed` wasn't passed at startup).
//!

use std::sync::Arc;

use axum::extract::{Extension, RawQuery};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use holofs_gateway::{Gateway, SearchBand};

use super::util::{error_to_response, parse_urlencoded_field};

/// `POST /api/embed_all` — kick off a one-shot bulk embed of every
/// image in the catalog that isn't in `embeddings.bin` yet.
/// Synchronous — the request hangs until the walk finishes — because
/// the typical run on a few hundred images is sub-minute and the
/// curl caller wants the final `(new, skipped)` counts to print.
pub async fn embed_all(Extension(gw): Extension<Arc<Gateway>>) -> Response {
    if !gw.embed_enabled().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "embed feature disabled — start the server with --enable-embed",
        )
            .into_response();
    }
    match gw.embed_all_pending().await {
        Ok((new_n, skip_n)) => (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "application/json")],
            format!("{{\"new\":{new_n},\"skipped\":{skip_n}}}"),
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// `GET /api/search?q=...&limit=...` — natural-language semantic
/// search. Returns JSON list of `{name, score}` sorted by cosine
/// descending.
pub async fn semantic_search(
    RawQuery(raw): RawQuery,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let Some(q) = raw.as_deref().and_then(|s| parse_urlencoded_field(s, "q")) else {
        return (StatusCode::BAD_REQUEST, "missing 'q' query param").into_response();
    };
    if q.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "'q' is empty").into_response();
    }
    let limit: usize = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "limit"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(200);
    if !gw.embed_enabled().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "embed feature disabled — start the server with --enable-embed",
        )
            .into_response();
    }
    let band = raw
        .as_deref()
        .and_then(|s| parse_urlencoded_field(s, "band"))
        .map(|s| SearchBand::parse(&s))
        .unwrap_or(SearchBand::Any);
    match gw.semantic_search(&q, limit, band).await {
        Ok(hits) => {
            let mut body = String::from("{\"hits\":[");
            for (i, h) in hits.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                let band_str = match h.band {
                    SearchBand::Coarse => "coarse",
                    SearchBand::Mid => "mid",
                    SearchBand::Full => "full",
                    SearchBand::Any => "any",
                };
                body.push_str(&format!(
                    "{{\"name\":\"{}\",\"score\":{:.6},\"band\":\"{band_str}\"}}",
                    h.name.replace('\\', "\\\\").replace('"', "\\\""),
                    h.score
                ));
            }
            body.push_str("]}");
            (
                StatusCode::OK,
                [(http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Err(e) => error_to_response(e),
    }
}
