//! Pure helpers shared across the axum handler modules.
//!
//! Nothing here touches the gateway state — all state-shaping code
//! (response builders that take `DecodedObject`, `IngestResult`,
//! `RemoveResult`, etc.) lives in `handlers/response.rs`.
//!
//! Every function is `pub(crate)` because the handler submodules
//! reach in via `use super::util::...` — nothing external calls
//! them.
//!

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use holofs_gateway::GatewayError;
use holofs_model::manifest::ObjectKind;

// === Small response builders ==============================================

pub(crate) fn bad_request(msg: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain")],
        msg,
    )
        .into_response()
}

/// String-owning variant of [`bad_request`]. Used by multipart-body
/// handlers where the error message includes the underlying parse
/// error (`format!(...)`), so a `&'static str` bound would be too
/// restrictive.
pub(crate) fn bad_request_owned(msg: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain")],
        msg,
    )
        .into_response()
}

pub(crate) fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        "not found",
    )
        .into_response()
}

/// Response shape for a GET/HEAD on an object still in
/// `ManifestState::Encoding`: 503 with `Retry-After: 5`. The message
/// body is short and machine-friendly so a polling client can log the
/// exact state without parsing HTML.
pub(crate) fn encoding_in_progress(name: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::RETRY_AFTER, "5"),
        ],
        format!("{name}: object is still encoding; retry after 5 s\n"),
    )
        .into_response()
}

/// Response shape for a GET/HEAD/DELETE on an object whose async
/// encode ran but failed. 404 with an explanatory body so the caller
/// can safely `PUT` again.
pub(crate) fn encoding_failed(name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        format!("{name}: previous PUT failed to encode; PUT again to replace\n"),
    )
        .into_response()
}

/// Build a JSON `Response` with the given status. Used by every
/// `/api/*` handler so payloads always advertise
/// `application/json`.
pub(crate) fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// 303 redirect to an absolute or relative URL. The form-friendly
/// mkdir/rmdir/upload handlers pick the target from a hidden
/// `return_to` field so each calling page can decide where to go
/// after success — the tree-view stays on `/`, the focus-view comes
/// back to `/?p=<parent>`, etc. Falls back to `/` if no target was
/// given.
pub(crate) fn redirect_to(target: &str) -> Response {
    let target = if target.is_empty() { "/" } else { target };
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, target)
        .body(Body::empty())
        .expect("redirect build")
}

/// Map a [`GatewayError`] to the appropriate HTTP status + text body.
/// The status matrix is documented in
/// [`holofs_gateway::GatewayError`]. Every writer handler funnels
/// its `Err(GatewayError)` branch through here.
pub(crate) fn error_to_response(e: GatewayError) -> Response {
    let (status, msg) = match &e {
        GatewayError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        GatewayError::BadRequest(s) => (StatusCode::BAD_REQUEST, s.clone()),
        GatewayError::Decode(s) => (StatusCode::SERVICE_UNAVAILABLE, s.clone()),
        GatewayError::PreviewUnsupported => (
            StatusCode::NOT_FOUND,
            "preview not supported for this kind".to_string(),
        ),
        GatewayError::IsDirectory => (StatusCode::CONFLICT, "is a directory".to_string()),
        GatewayError::AlreadyExists => {
            (StatusCode::CONFLICT, "already exists".to_string())
        }
        GatewayError::NotADirectory => {
            (StatusCode::CONFLICT, "not a directory".to_string())
        }
        GatewayError::DirectoryNotEmpty => {
            (StatusCode::CONFLICT, "directory not empty".to_string())
        }
        GatewayError::ClusterDegraded => (
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster has no live nodes".to_string(),
        ),
        GatewayError::Persist(s) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("catalog persist failed: {s}"),
        ),
    };
    (status, [(header::CONTENT_TYPE, "text/plain")], msg).into_response()
}

// === Path / form parsing ==================================================

/// Top-level path segments owned by the HTTP frontend itself. An
/// object (or any of its parent directories) named with one of
/// these would shadow a real route, so PUT/DELETE/mkdir refuse
/// them.
const RESERVED_TOP_SEGMENTS: &[&str] = &[
    "health", "escrow", "preview", "inspect", "similar", "diff", "admin", "api",
    "metrics", "pkg",
    // in-app docs viewer + zoom variant of /inspect.
    "help", "inspect-zoom",
    // static-asset prefix served by ServeDir.
    "assets",
    // wavelet-mix composer page.
    "mix",
    // pitch / marketing page.
    "about",
    // semantic search page.
    "search",
    // streaming hologram demo page.
    "holo",
    // ROI spotlight composite page.
    "spotlight",
    // per-object version history page.
    "versions",
];

pub(crate) fn top_segment(path: &str) -> &str {
    path.split('/').next().unwrap_or("")
}

/// `true` if `name`'s top-level segment collides with an HTTP route.
/// Empty name is also reserved (handled separately by path
/// validation).
#[allow(dead_code)]
pub(crate) fn is_reserved_name(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    RESERVED_TOP_SEGMENTS.contains(&top_segment(name))
}

/// Names accepted by `PUT /<path>` and the directory-op endpoints.
/// Slash is allowed (paths are multi-segment after ); only
/// the top-level segment is checked against the reserved list.
/// Structural validation (dot/double-slash/length) is the gateway's
/// job — `Gateway::ingest_bytes` runs `holofs_model::path::validate`
/// and returns `BadRequest` on failure.
pub(crate) fn is_valid_put_name(name: &str) -> bool {
    !name.is_empty() && !RESERVED_TOP_SEGMENTS.contains(&top_segment(name))
}

/// Minimal `application/x-www-form-urlencoded` field extractor.
/// Good enough for the single-field admin-node form.
pub(crate) fn parse_urlencoded_field(body: &str, field: &str) -> Option<String> {
    for pair in body.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next()?;
        let val = it.next().unwrap_or("");
        if key == field {
            return Some(url_decode_simple(val));
        }
    }
    None
}

/// Decode `+` → space and `%XX` → byte for a single form field.
///
/// bytes flow through a `Vec<u8>` rather than being
/// pushed straight into a `String`. The old code did
/// `out.push(byte as char)`, which treated each decoded byte as a
/// Unicode code point — fine for ASCII, garbage for any multi-byte
/// UTF-8 sequence. A Russian "С" (UTF-8 `D0 A1`) used to render as
/// `Ð¡`; with the byte-buffer path we reassemble the original UTF-8
/// bytes and `String::from_utf8_lossy` hands back the right
/// characters.
pub(crate) fn url_decode_simple(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                } else {
                    out.push(b'%');
                }
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tiny URL-encode for redirect targets — only escapes the few
/// characters that mangle a `/?open=` query value. Kept local to
/// avoid pulling another crate.
pub(crate) fn url_encode_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Resolve where to send the user after a form mutation. Honours an
/// explicit `return_to` field when present (the tree-view forms set
/// this to `/` so the page doesn't switch to focus mode); otherwise
/// falls back to `/?p=<parent>` for backward-compat with the
/// focus-view forms that omit the field.
pub(crate) fn pick_return_to(return_to: &str, parent: &str) -> String {
    if !return_to.is_empty() {
        return return_to.to_string();
    }
    if parent.is_empty() {
        "/".to_string()
    } else {
        format!("/?p={parent}")
    }
}

/// Escape a string for inline JSON (object names go straight into a
/// JSON response so we need to neutralise quotes and backslashes).
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// === Header helpers =======================================================

/// Compile-time header-name shortcut. Every `X-Holofs-*` custom
/// header has to route through `HeaderName::from_static` (which
/// panics on invalid input at boot time only for these fixed
/// strings, not at runtime).
pub(crate) fn x(s: &'static str) -> HeaderName {
    HeaderName::from_static(s)
}

pub(crate) fn kind_label(kind: ObjectKind) -> HeaderValue {
    HeaderValue::from_static(match kind {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    })
}

// === Misc ================================================================

/// Turn arbitrary bytes into a square grayscale PNG: side =
/// ceil(sqrt(len)), tail padded with zeros. Mirrors the legacy
/// gateway's `render_shard_as_png`.
pub(crate) fn render_shard_as_png(bytes: &[u8]) -> Vec<u8> {
    let side = (bytes.len() as f64).sqrt().ceil() as usize;
    let side = side.max(1);
    let total = side * side;
    let mut padded = bytes.to_vec();
    padded.resize(total, 0);
    let mut rgb = Vec::with_capacity(total * 3);
    for &g in &padded {
        rgb.extend_from_slice(&[g, g, g]);
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, side as u32, side as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&rgb).expect("png write");
    }
    buf
}
