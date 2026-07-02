//! Multipart upload axum handler.
//!
//! - `POST /api/upload` — [`upload_form`]. Multipart body with
//!   `parent` (may be empty for root), optional `name` override,
//!   `file` (binary), and `return_to`. Runs
//!   [`Gateway::ingest_bytes`] against `parent/<name>` and 303-
//!   redirects to the target picked by
//!   [`super::util::pick_return_to`]. The catalog page's
//!   drag-zone-style form invokes this so users can add files
//!   without resorting to `curl -X PUT`.
//!
//! Moved out of `handlers.rs` in Phase R2b.5.

use std::sync::Arc;

use axum::extract::{Extension, Multipart};
use axum::response::Response;

use holofs_gateway::Gateway;

use super::util::{
    bad_request, bad_request_owned, error_to_response, is_valid_put_name, pick_return_to,
    redirect_to,
};

/// `POST /api/upload` (multipart) — form-friendly object upload.
/// Fields: `parent` (string, may be empty for root), `name`
/// (optional override), `file` (binary). The destination path
/// becomes `parent/<name or file.name>`. Redirects back to
/// `/?p=<parent>` on success.
pub async fn upload_form(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut parent = String::new();
    let mut name_override: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;
    let mut return_to_field = String::new();
    while let Ok(Some(field)) = form.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        let upload_filename = field.file_name().map(|s| s.to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        match field_name.as_str() {
            "parent" => parent = String::from_utf8_lossy(&bytes).to_string(),
            "name" => {
                let s = String::from_utf8_lossy(&bytes).trim().to_string();
                if !s.is_empty() {
                    name_override = Some(s);
                }
            }
            "file" => {
                file_bytes = Some(bytes.to_vec());
                original_filename = upload_filename.filter(|f| !f.is_empty());
            }
            "return_to" => {
                return_to_field = String::from_utf8_lossy(&bytes).trim().to_string();
            }
            _ => {}
        }
    }
    let Some(body) = file_bytes else {
        return bad_request("no file field");
    };
    if body.is_empty() {
        return bad_request("empty file");
    }
    let leaf = name_override
        .or(original_filename)
        .unwrap_or_else(|| "uploaded.bin".to_string());
    let path = if parent.is_empty() {
        leaf
    } else {
        format!("{parent}/{leaf}")
    };
    if !is_valid_put_name(&path) {
        return bad_request("reserved or empty top segment");
    }
    let target = pick_return_to(&return_to_field, &parent);
    match gw.ingest_bytes(&path, &body).await {
        Ok(_) => {
            gw.embed_object_in_background(path);
            redirect_to(&target)
        }
        Err(e) => error_to_response(e),
    }
}
