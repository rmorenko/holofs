//! Small helpers shared across the Gateway modules.
//!
//! These are pulled out of the main `http_gateway.rs` monolith so
//! per-file compile time stays reasonable and every dependency
//! (content-type sniffers, PNG encoder, path helpers) has one
//! obvious home.

use holofs_codec::image_io::to_rgb;
use holofs_core::hash::sha256;

/// Current time in seconds since Unix epoch. Thin re-export of
/// [`holofs_core::time::now_unix`] — kept for `pub(crate)` visibility
/// so `use crate::util::now_unix` continues to work across the
/// gateway modules unchanged.
pub(crate) fn now_unix() -> u64 {
    holofs_core::time::now_unix()
}

/// Stable, path-derived id for directory markers. We hash with a
/// separate domain tag so a collision with a data object id is
/// impossible.
pub(crate) fn directory_object_id(path: &str) -> u64 {
    let mut buf = Vec::with_capacity(path.len() + 16);
    buf.extend_from_slice(b"holofs-dir-v1\0");
    buf.extend_from_slice(path.as_bytes());
    let h = sha256(&buf);
    let mut id = [0u8; 8];
    id.copy_from_slice(&h[..8]);
    u64::from_be_bytes(id)
}

/// Guess the MIME type of an opaque blob by file extension.
/// Falls back to `application/octet-stream` for unknowns.
pub(crate) fn guess_opaque_content_type(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ppt" => "application/vnd.ms-powerpoint",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "gzip" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "epub" => "application/epub+zip",
        "mobi" => "application/x-mobipocket-ebook",
        "rtf" => "application/rtf",
        "sqlite" | "db" => "application/vnd.sqlite3",
        "exe" | "dll" => "application/vnd.microsoft.portable-executable",
        "dmg" => "application/x-apple-diskimage",
        "iso" => "application/x-iso9660-image",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Guess the content-type of a text file by name (for use on GET).
pub(crate) fn guess_text_content_type(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown; charset=utf-8".into()
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8".into()
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8".into()
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8".into()
    } else if lower.ends_with(".csv") {
        "text/csv; charset=utf-8".into()
    } else {
        "text/plain; charset=utf-8".into()
    }
}

/// PNG-encode three float channels of shape `w × h` back into an
/// RGB image buffer. Used by every decode path that ends in an
/// HTTP `image/png` response body.
pub fn encode_png(channels: &[Vec<f32>], w: u32, h: u32) -> Vec<u8> {
    let arr: [Vec<f32>; 3] = [
        channels[0].clone(),
        channels[1].clone(),
        channels[2].clone(),
    ];
    let rgb = to_rgb(&arr, w as usize, h as usize);
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("PNG header write");
        writer
            .write_image_data(&rgb)
            .expect("PNG data write");
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_object_id_is_deterministic_and_path_sensitive() {
        let a1 = directory_object_id("photos");
        let a2 = directory_object_id("photos");
        let b = directory_object_id("photos/2026");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_ne!(a1, 0);
    }
}
