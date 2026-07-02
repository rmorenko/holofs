//! Directory-mutation axum handlers.
//!
//! Six routes wrapping the [`Gateway::mkdir`], [`Gateway::rmdir`],
//! [`Gateway::remove_object`], and [`Gateway::rename`] APIs:
//!
//! - `POST /api/mkdir/*path` — JSON mkdir.
//! - `POST /api/mkdir` (form) — form-friendly mkdir with redirect.
//! - `DELETE /api/rmdir/*path` — JSON rmdir.
//! - `POST /api/rmdir` (form) — form-friendly rmdir with redirect.
//! - `POST /api/rm` (form) — form-friendly file delete.
//! - `POST /api/mv` (form) — rename / move.
//!
//! The form-friendly variants read a `return_to` field to pick the
//! post-success redirect target — see [`super::util::pick_return_to`].
//!
//! Moved out of `handlers.rs` in Phase R2b.4.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::response::Response;

use holofs_gateway::Gateway;

use super::response::{mkdir_to_response, rename_to_response, rmdir_to_response};
use super::util::{
    bad_request, error_to_response, is_valid_put_name, parse_urlencoded_field,
    pick_return_to, redirect_to,
};

/// `POST /api/mkdir/*path` — create a `Directory` marker at `path`.
/// Returns JSON `{path, object_id}` on success, 409 on conflict, 400
/// on bad input.
pub async fn mkdir(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if !is_valid_put_name(&name) {
        return bad_request("reserved or empty top segment");
    }
    match gw.mkdir(&name).await {
        Ok(res) => mkdir_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/mkdir` (form-urlencoded `parent=&name=`) — form-friendly
/// variant invoked by the inline "new folder" form on the catalog
/// page. Joins `parent` + `name`, runs the same mkdir, then
/// 303-redirects back to `/?p=<parent>` so the browser reloads with
/// the new tile visible.
pub async fn mkdir_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let parent = parse_urlencoded_field(body_str, "parent").unwrap_or_default();
    let Some(name) = parse_urlencoded_field(body_str, "name") else {
        return bad_request("missing 'name'");
    };
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let path = if parent.is_empty() {
        name
    } else {
        format!("{parent}/{name}")
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let target = pick_return_to(&return_to_field, &parent);
    match gw.mkdir(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// `DELETE /api/rmdir/*path` — remove an empty directory entry.
/// Returns `{path, object_id}`; 409 if the directory still has
/// children.
pub async fn rmdir(
    Path(name): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    if !is_valid_put_name(&name) {
        return bad_request("reserved or empty top segment");
    }
    match gw.rmdir(&name).await {
        Ok(res) => rmdir_to_response(res),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/rm` (form-urlencoded `path=`) — form-friendly variant of
/// `DELETE /<name>` for the ✕ button on file rows. Redirects to
/// `return_to` (or the parent dir) on success. Mirrors [`rmdir_form`]
/// but resolves to `remove_object` instead of `rmdir`. Stage 11.17.
pub async fn rm_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(path) = parse_urlencoded_field(body_str, "path") else {
        return bad_request("missing 'path'");
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = pick_return_to(&return_to_field, &parent);
    match gw.remove_object(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/rmdir` (form-urlencoded `path=`) — form-friendly variant
/// for the delete button on directory cards. Redirects back to the
/// parent directory on success.
pub async fn rmdir_form(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(path) = parse_urlencoded_field(body_str, "path") else {
        return bad_request("missing 'path'");
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let return_to_field = parse_urlencoded_field(body_str, "return_to").unwrap_or_default();
    let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let target = pick_return_to(&return_to_field, &parent);
    match gw.rmdir(&path).await {
        Ok(_) => redirect_to(&target),
        Err(e) => error_to_response(e),
    }
}

/// `POST /api/mv` — rename / move an entry. Body is form-urlencoded
/// `from=...&to=...` so the dropzone HTML form can submit it without
/// JS. Directories carry every descendant along.
pub async fn mv(
    Extension(gw): Extension<Arc<Gateway>>,
    body: Bytes,
) -> Response {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return bad_request("non-utf8 body"),
    };
    let Some(from) = parse_urlencoded_field(body_str, "from") else {
        return bad_request("missing 'from'");
    };
    let Some(to) = parse_urlencoded_field(body_str, "to") else {
        return bad_request("missing 'to'");
    };
    if !is_valid_put_name(&from) || !is_valid_put_name(&to) {
        return bad_request("reserved or empty top segment");
    }
    match gw.rename(&from, &to).await {
        Ok(res) => rename_to_response(res),
        Err(e) => error_to_response(e),
    }
}
