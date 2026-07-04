//! Response builders that project gateway result structs onto
//! `axum::response::Response`.
//!
//! Every writer handler (`ingest_bytes`, `remove_object`, `mkdir`,
//! `rmdir`, `rename`) returns a domain struct from
//! `holofs_gateway::*`; this module owns the JSON serialisation +
//! header shape for those structs. The read-side responses
//! ([`serve_with_range`], [`decoded_to_response`],
//! [`partial_response`], [`unsatisfiable_response`]) live here too —
//! they share the same `X-Holofs-*` header vocabulary.
//!
//! [`stats_to_json`] and [`fingerprint_to_json`] format the two
//! JSON view-model responses (`/api/stats`, `/api/fingerprint/*`).

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use holofs_gateway::{
    ApiStats, DecodedObject, FingerprintInfo, IngestResult, KindCounts, MkdirResult,
    RemoveResult, RenameResult, RmdirResult,
};
use holofs_model::manifest::ObjectKind;

use crate::range::{parse_range, ByteRange, RangeOutcome};

use super::util::{json_escape, kind_label, x};

/// Dispatch based on the request's `Range` header:
/// - none / malformed → full 200 body via [`decoded_to_response`];
/// - single satisfiable range → 206 via [`partial_response`];
/// - unsatisfiable → 416 with `Content-Range: bytes */<total>`;
/// - multi-range → degrade gracefully to a full 200.
///
/// The full decoded buffer is always materialised first — partial
/// reads are slice operations, not progressive decode. This
/// matches the rest of the gateway's read path and is enough for
/// the workloads Range actually helps with (resume, `<audio>`
/// scrubbing).
pub(crate) fn serve_with_range(name: &str, obj: DecodedObject, headers: &HeaderMap) -> Response {
    let total = obj.bytes.len() as u64;
    let outcome = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| parse_range(s, total))
        .unwrap_or(RangeOutcome::NoRange);
    match outcome {
        RangeOutcome::NoRange | RangeOutcome::Multiple | RangeOutcome::Malformed => {
            decoded_to_response(name, obj)
        }
        RangeOutcome::Range(r) => partial_response(name, obj, r),
        RangeOutcome::Unsatisfiable => unsatisfiable_response(total),
    }
}

/// 416 Range Not Satisfiable. Per RFC 9110 §15.5.17 the response
/// MUST carry a `Content-Range: bytes */<total>` so the client
/// knows the resource's true size and can retry sensibly.
pub(crate) fn unsatisfiable_response(total: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body("range not satisfiable".into())
        .expect("416 build")
}

/// 206 Partial Content. Copies the requested slice out of
/// `obj.bytes`, emits the standard `Content-Range: bytes A-B/total`
/// header, and preserves the kind / cache / disposition headers
/// from a full response.
pub(crate) fn partial_response(name: &str, obj: DecodedObject, range: ByteRange) -> Response {
    let total = obj.bytes.len() as u64;
    let start = range.start as usize;
    let end_inclusive = range.end_inclusive as usize;
    let slice = obj.bytes[start..=end_inclusive].to_vec();
    let slice_len = slice.len();

    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, &obj.content_type)
        .header(header::CONTENT_LENGTH, slice_len)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{total}", range.start, range.end_inclusive),
        )
        .header(x("x-holofs-kind"), kind_label(obj.kind))
        .header(x("x-holofs-bytes-downloaded"), obj.bytes_downloaded)
        .header(x("x-holofs-decode-ms"), obj.decode_ms as u64);

    if let Some(layer) = obj.max_layer {
        builder = builder.header(x("x-holofs-layers"), format!("0-{layer}"));
    }
    if let Some(sr) = obj.sample_rate {
        builder = builder.header(x("x-holofs-sample-rate"), sr);
    }
    if let Some(ch) = obj.channels {
        builder = builder.header(x("x-holofs-channels"), u16::from(ch));
    }
    if obj.kind == ObjectKind::Image {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=3600");
    }
    if let Some(filename) = obj.filename_for_disposition.as_deref() {
        let safe = filename.replace('"', "");
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\""),
        );
    }
    let _ = name;
    builder.body(slice.into()).expect("206 build")
}

pub(crate) fn decoded_to_response(name: &str, obj: DecodedObject) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &obj.content_type)
        .header(header::CONTENT_LENGTH, obj.bytes.len())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(x("x-holofs-kind"), kind_label(obj.kind))
        .header(x("x-holofs-bytes-downloaded"), obj.bytes_downloaded)
        .header(x("x-holofs-decode-ms"), obj.decode_ms as u64);

    if let Some(layer) = obj.max_layer {
        builder = builder.header(x("x-holofs-layers"), format!("0-{layer}"));
    }
    if let Some(sr) = obj.sample_rate {
        builder = builder.header(x("x-holofs-sample-rate"), sr);
    }
    if let Some(ch) = obj.channels {
        builder = builder.header(x("x-holofs-channels"), u16::from(ch));
    }
    if let Some(total) = obj.chunks_total {
        builder = builder.header(x("x-holofs-chunks-total"), total as u64);
    }
    if let Some(missing) = obj.chunks_missing {
        builder = builder.header(x("x-holofs-chunks-missing"), missing as u64);
    }
    if obj.kind == ObjectKind::Image {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=3600");
    }
    if let Some(filename) = obj.filename_for_disposition.as_deref() {
        let safe = filename.replace('"', "");
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\""),
        );
    }
    let _ = name;
    builder.body(obj.bytes.into()).expect("response build")
}

pub(crate) fn ingest_to_response(res: IngestResult) -> Response {
    let body = format!(
        "{{\"name\":\"{name}\",\
\"object_id\":\"{oid:016x}\",\
\"data_cid\":\"{cid}\",\
\"width\":{w},\"height\":{h},\
\"shards\":{shards},\
\"put_ms\":{ms}}}\n",
        name = json_escape(&res.name),
        oid = res.object_id,
        cid = res.data_cid_hex,
        w = res.width,
        h = res.height,
        shards = res.total_shards,
        ms = res.put_ms,
    );
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub(crate) fn remove_to_response(res: RemoveResult) -> Response {
    let body = format!(
        "{{\"deleted\":\"{name}\",\"object_id\":\"{oid:016x}\"}}\n",
        name = json_escape(&res.name),
        oid = res.object_id,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub(crate) fn mkdir_to_response(res: MkdirResult) -> Response {
    let body = format!(
        "{{\"created\":\"{p}\",\"object_id\":\"{oid:016x}\"}}\n",
        p = json_escape(&res.path),
        oid = res.object_id,
    );
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub(crate) fn rmdir_to_response(res: RmdirResult) -> Response {
    let body = format!(
        "{{\"removed\":\"{p}\",\"object_id\":\"{oid:016x}\"}}\n",
        p = json_escape(&res.path),
        oid = res.object_id,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub(crate) fn rename_to_response(res: RenameResult) -> Response {
    let body = format!(
        "{{\"from\":\"{f}\",\"to\":\"{t}\",\"moved\":{n}}}\n",
        f = json_escape(&res.old),
        t = json_escape(&res.new),
        n = res.moved_entries,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// JSON serialisation for `/api/stats`. Manually written because
/// the response is small, flat, and the existing snapshot tests
/// diff against the exact byte sequence.
pub(crate) fn stats_to_json(s: &ApiStats) -> String {
    let KindCounts {
        image,
        audio,
        text,
        opaque,
        directory,
    } = s.objects_by_kind;
    format!(
        "{{\"nodes_total\":{nt},\
\"nodes_live\":{nl},\
\"objects_total\":{ot},\
\"objects_by_kind\":{{\"image\":{image},\"audio\":{audio},\"text\":{text},\"opaque\":{opaque},\"directory\":{directory}}},\
\"shards_total\":{sht},\
\"shards_unique\":{shu},\
\"dedup_savings_pct\":{dd:.2},\
\"bytes_total\":{bt},\
\"auto_repairs_total\":{ar},\
\"auto_repair_failures_total\":{arf},\
\"scrub_runs_total\":{srt},\
\"scrub_repairs_total\":{srpt}}}\n",
        nt = s.nodes_total,
        nl = s.nodes_live,
        ot = s.objects_total,
        sht = s.shards_total,
        shu = s.shards_unique,
        dd = s.dedup_savings_pct,
        bt = s.bytes_total,
        ar = s.auto_repairs_total,
        arf = s.auto_repair_failures_total,
        srt = s.scrub_runs_total,
        srpt = s.scrub_repairs_total,
    )
}

pub(crate) fn fingerprint_to_json(info: &FingerprintInfo) -> String {
    format!(
        "{{\"name\":\"{n}\",\"fingerprint\":\"{fp}\",\"kind\":\"{k}\"}}\n",
        n = json_escape(&info.name),
        fp = info.fingerprint_hex,
        k = match info.kind {
            ObjectKind::Image => "image",
            ObjectKind::Audio => "audio",
            ObjectKind::Text => "text",
            ObjectKind::Opaque => "opaque",
            ObjectKind::Directory => "directory",
        },
    )
}
