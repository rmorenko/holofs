//! Stage 8 holographic key escrow — axum handlers.
//!
//! Three routes wrap the [`holofs_gateway::Gateway`] escrow API:
//!
//! - `POST /escrow/split` — multipart `file` + `k` + `n` → in-memory
//!   shares. Renders the same server-side view that lives in
//!   [`crate::escrow`] so the result page keeps the site's stylesheet
//!   + i18n locale.
//! - `GET /escrow/download/:id_idx` — one encoded `.holoshare`.
//! - `POST /escrow/recover` — multipart with one or more `shares=…`
//!   files; returns the recovered file with the original
//!   `Content-Type` + `Content-Disposition: attachment`.
//!
//! `bad_request_owned` (also used by the multipart-body upload
//! handlers) lives in [`super`] and is re-imported here.
//!
//! Moved out of `handlers.rs` in Phase R2b.1.

use std::sync::Arc;

use axum::extract::{Extension, Multipart, Path};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};

use holofs_gateway::{
    EscrowRecoverResult, EscrowShareBytes, EscrowSplitResult, Gateway, GatewayError,
};

use super::util::{bad_request, bad_request_owned, error_to_response};

/// `POST /escrow/split` — multipart `file` + `k` + `n` → in-memory shares.
/// Renders an HTML result page listing each `.holoshare` download link.
pub async fn escrow_split(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename = String::from("secret.bin");
    let mut k: usize = 3;
    let mut n: usize = 5;
    // Stage 11.19b: the form ships a hidden `lang` field carrying the
    // current page locale so the server-rendered result page matches
    // the language the user saw on `/escrow`. Falls back to `en` when
    // the field is absent or unknown.
    let mut lang = String::from("en");
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().map(|s| s.to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        match name.as_str() {
            "file" => {
                if let Some(fname) = fname {
                    if !fname.is_empty() {
                        filename = fname;
                    }
                }
                file_bytes = Some(bytes.to_vec());
            }
            "k" => {
                if let Some(v) = parse_form_usize(&bytes) {
                    k = v;
                }
            }
            "n" => {
                if let Some(v) = parse_form_usize(&bytes) {
                    n = v;
                }
            }
            "lang" => {
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    let trimmed = s.trim();
                    if crate::i18n::is_known_locale(trimmed) {
                        lang = trimmed.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    let Some(file_bytes) = file_bytes else {
        return bad_request("no file field");
    };
    match gw.escrow_split(file_bytes, filename, k, n).await {
        Ok(res) => escrow_split_html(res, &lang),
        Err(e) => error_to_response(e),
    }
}

/// `GET /escrow/download/:id_idx` — return one encoded `.holoshare`. The
/// path segment is `<escrow_id_hex>_<idx>.holoshare`; the trailing
/// `.holoshare` is stripped server-side.
pub async fn escrow_download(
    Path(path): Path<String>,
    Extension(gw): Extension<Arc<Gateway>>,
) -> Response {
    let stem = path.trim_end_matches(".holoshare");
    let Some((eid_hex, idx_str)) = stem.rsplit_once('_') else {
        return bad_request("bad escrow download path");
    };
    let Ok(idx) = idx_str.parse::<usize>() else {
        return bad_request("bad share index");
    };
    match gw.escrow_download(eid_hex, idx).await {
        Ok(share) => escrow_share_response(share),
        Err(GatewayError::NotFound) => (
            StatusCode::GONE,
            [(header::CONTENT_TYPE, "text/plain")],
            "escrow gone (gateway restarted — split the file again)",
        )
            .into_response(),
        Err(e) => error_to_response(e),
    }
}

/// `POST /escrow/recover` — multipart with one or more `shares=...` files.
/// Returns the recovered file with the original `Content-Type` and
/// `Content-Disposition: attachment`.
pub async fn escrow_recover(
    Extension(gw): Extension<Arc<Gateway>>,
    mut form: Multipart,
) -> Response {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return bad_request_owned(format!("multipart read: {e}")),
        };
        if name == "shares" && !bytes.is_empty() {
            blobs.push(bytes.to_vec());
        }
    }
    match gw.escrow_recover(blobs).await {
        Ok(res) => escrow_recover_response(res),
        Err(e) => error_to_response(e),
    }
}

fn escrow_split_html(res: EscrowSplitResult, lang: &str) -> Response {
    use crate::escrow::{EscrowShareRow, EscrowSplitResultView};
    use crate::i18n::{translate, LocaleSignal};
    use leptos::prelude::*;

    let EscrowSplitResult {
        filename,
        source_bytes,
        k,
        n,
        escrow_id_hex,
        shares,
    } = res;
    let rows: Vec<EscrowShareRow> = shares
        .into_iter()
        .map(|s| EscrowShareRow {
            idx: usize::from(s.idx),
            filename: s.filename,
            bytes: s.bytes as u64,
            download_path: s.download_path,
        })
        .collect();
    let lang_owned = lang.to_string();
    let lang_for_view = lang_owned.clone();
    let lang_for_shell = lang_owned.clone();

    // Stage 11.19b: render through the same `view!` pipeline the rest
    // of the site uses, then wrap in a manual document shell. We need
    // the manual shell because this response isn't routed through the
    // Leptos router — it's a direct POST result, so we can't reuse the
    // `App` shell which builds `<Router>` + `<RoutedApp>`.
    let owner = Owner::new();
    let body_html: String = owner.with(|| {
        // Provide a LocaleSignal so every `t!()` inside the view picks
        // up the request locale instead of falling back to "en".
        provide_context(LocaleSignal(Signal::derive(move || lang_for_view.clone())));
        view! {
            <EscrowSplitResultView
                filename=filename.clone()
                source_bytes=source_bytes as u64
                k=k
                n=n
                escrow_id_hex=escrow_id_hex.clone()
                shares=rows.clone()
            />
        }
        .to_html()
    });

    let title = translate("escrow.result.title_tag", &lang_for_shell);
    let body = format!(
        r#"<!DOCTYPE html><html lang="{lang_for_shell}"><head>\
<meta charset="utf-8"/>\
<meta name="viewport" content="width=device-width, initial-scale=1"/>\
<script>(function(){{try{{var t=localStorage.getItem('holofs-theme')||'dark';\
document.documentElement.dataset.theme=t;}}catch(e){{}}}})();</script>\
<link rel="stylesheet" href="/pkg/holofs.css"/>\
<title>{title}</title>\
</head><body>{body_html}</body></html>
"#,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn escrow_share_response(share: EscrowShareBytes) -> Response {
    let EscrowShareBytes { idx, total_n, bytes } = share;
    let filename = format!("share_{idx:02}_of_{total_n}.holoshare");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(bytes.into())
        .expect("share response build")
}

fn escrow_recover_response(res: EscrowRecoverResult) -> Response {
    let EscrowRecoverResult {
        data,
        content_type,
        filename,
        shares_used,
    } = res;
    let safe_filename = filename.replace('"', "");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, data.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe_filename}\""),
        )
        .header(
            HeaderName::from_static("x-holofs-escrow-shares-used"),
            shares_used as u64,
        )
        .body(data.into())
        .expect("recover response build")
}

fn parse_form_usize(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}
