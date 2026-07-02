//! Stage 13.4 — form-friendly version-history axum handlers.
//!
//! - `POST /api/restore` — [`restore_version_form`]. Body
//!   `name=<path>&id=<version_id>&return_to=<url>`. On success
//!   303-redirects to `return_to` (defaults to `/versions/<name>`).
//! - `POST /api/versions/delete` — [`delete_version_form`]. Body
//!   `name=<path>&id=<version_id>&return_to=<url>`. On success
//!   303-redirects to `return_to`. The deleted version's uniquely-
//!   owned shards are GC'd from the cluster in the same call.
//!
//! Moved out of `handlers.rs` in Phase R2b.5.

use std::sync::Arc;

use axum::extract::Extension;
use axum::response::Response;

use holofs_gateway::Gateway;

use super::util::{
    bad_request, error_to_response, parse_urlencoded_field, redirect_to, url_encode_simple,
};

/// Stage 13.4: `POST /api/restore` — form-friendly version restore.
/// Body: `name=<path>&id=<version_id>&return_to=<url>`. On success
/// 303-redirects to `return_to` (defaults to `/versions/<name>`).
pub async fn restore_version_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: String,
) -> Response {
    let Some(name) = parse_urlencoded_field(&body, "name") else {
        return bad_request("missing 'name'");
    };
    let Some(id) = parse_urlencoded_field(&body, "id") else {
        return bad_request("missing 'id'");
    };
    let return_to = parse_urlencoded_field(&body, "return_to")
        .unwrap_or_else(|| format!("/versions/{}", url_encode_simple(&name)));
    match gw.restore_version(&name, &id).await {
        Ok(_) => {
            // Drop any cached preview / WAV for this name now that the
            // catalog points at a different manifest.
            gw.invalidate_cache(&name).await;
            redirect_to(&return_to)
        }
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/versions/delete` — form-friendly version deletion.
/// Body: `name=<path>&id=<version_id>&return_to=<url>`. On success
/// 303-redirects to `return_to` (defaults to `/versions/<name>`).
/// The deleted version's uniquely-owned shards are GC'd from the
/// cluster in the same call.
pub async fn delete_version_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: String,
) -> Response {
    let Some(name) = parse_urlencoded_field(&body, "name") else {
        return bad_request("missing 'name'");
    };
    let Some(id) = parse_urlencoded_field(&body, "id") else {
        return bad_request("missing 'id'");
    };
    let return_to = parse_urlencoded_field(&body, "return_to")
        .unwrap_or_else(|| format!("/versions/{}", url_encode_simple(&name)));
    match gw.delete_version(&name, &id).await {
        Ok(_) => redirect_to(&return_to),
        Err(e) => error_to_response(e),
    }
}
