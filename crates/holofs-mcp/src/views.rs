//! MCP tool I/O view-models.
//!
//! Every request/response DTO that crosses the MCP boundary lives
//! here. Structs are grouped by tool family (catalog / inspection /
//! writes / transforms) and derive `schemars::JsonSchema` so `rmcp`
//! can generate MCP tool schemas straight from serde definitions.

use serde::{Deserialize, Serialize};

use rmcp::schemars;

// ===== Catalog + read tools =============================================

/// Input for `list_catalog`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListCatalogIn {
    /// Directory prefix to list (e.g. `"photos/2026"`). Empty / omitted
    /// lists the catalog root.
    #[serde(default)]
    pub prefix: Option<String>,
    /// `true` returns the entire subtree rooted at `prefix`; `false`
    /// (default) returns only direct children.
    #[serde(default)]
    pub recursive: bool,
}

/// One row of the `list_catalog` result. `size_bytes` is not tracked on
/// the manifest — the closest proxy is `n_shards` (the systematic-shard
/// count, ≈ original byte count divided by `sym_len`). Width / height /
/// sample rate are populated for image / audio kinds.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CatalogRow {
    pub path: String,
    /// `"image" | "audio" | "text" | "opaque" | "directory"`.
    pub kind: String,
    pub content_type: String,
    pub n_shards: u32,
    pub width: u32,
    pub height: u32,
    pub audio_sample_rate: u32,
    pub created_at_unix: u64,
}

/// Output for `list_catalog`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListCatalogOut {
    pub prefix: String,
    pub recursive: bool,
    pub count: usize,
    pub entries: Vec<CatalogRow>,
}

/// Input for `read_object_text`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadTextIn {
    /// Catalog path of a text-kind object.
    pub path: String,
}

/// Output for `read_object_text`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadTextOut {
    pub path: String,
    /// `true` when the returned `content` was truncated at
    /// `crate::util::MAX_READ_TEXT_BYTES`.
    pub truncated: bool,
    pub total_bytes: u64,
    pub content: String,
}

/// Input for `find_similar`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SimilarIn {
    pub path: String,
    /// `"all"` (default) | `"folder"` | `"tree"`. See Stage 11.16.
    #[serde(default)]
    pub scope: Option<String>,
}

/// One neighbour row of `find_similar`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SimilarRow {
    pub path: String,
    pub similarity_pct: f32,
    /// `"Jaccard"` (text) | `"DHash"` (image/audio).
    pub method: String,
}

/// Output for `find_similar`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SimilarOut {
    pub target: String,
    pub kind: String,
    pub scope: String,
    pub neighbors: Vec<SimilarRow>,
}

/// Input for `get_object_health`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ObjectHealthIn {
    pub path: String,
}

/// One node row of the cluster index.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NodeRow {
    pub idx: usize,
    pub addr: String,
    pub zone: u8,
    pub admin_killed: bool,
}

/// Output for `get_health` when no `path` is supplied.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct HealthIndexOut {
    /// Live nodes (out of total) — `kill -9 admin` flag honoured.
    pub n_live: usize,
    pub n_total: usize,
    pub nodes: Vec<NodeRow>,
    /// Catalog names known to the cluster, sorted alphabetically.
    pub objects: Vec<String>,
}

// ===== Write tools ======================================================

/// Input for `put_object_text` — write a UTF-8 string to a catalog path.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PutTextIn {
    /// Catalog path. Parent directories must already exist; create them
    /// with `mkdir` first.
    pub path: String,
    /// UTF-8 string body. Must be non-empty.
    pub content: String,
    /// Optional MIME override (defaults to `text/plain; charset=utf-8`).
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Result of `put_object_text` / `mkdir` / `mv_object`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WriteOk {
    pub path: String,
    /// Free-form summary — `"created"`, `"renamed"`, `"updated"`, etc.
    pub action: String,
    /// Optional detail (object_id hex, shard count, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Input for `mkdir` / `rmdir`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DirIn {
    pub path: String,
}

/// Input for `mv_object`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MoveIn {
    pub from: String,
    pub to: String,
}

// ===== Transform tools (wavelet_mix + audio_filter) =====================

/// Input for `wavelet_mix`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WaveletMixIn {
    /// Catalog path of source A (provides layers `0..=split`).
    pub a: String,
    /// Catalog path of source B (provides layers `>split`).
    pub b: String,
    /// DWT split layer. `0` ⇒ only L0 comes from A and everything else
    /// from B (heavy structural transfer); `nlayers-1` ⇒ entirely A.
    pub split: u8,
    /// Optional catalog path to ingest the result at. Requires
    /// `HOLOFS_MCP_TOKEN`; with no token the call falls through and
    /// just returns the bytes inline.
    #[serde(default)]
    pub save_as: Option<String>,
}

/// Output for `wavelet_mix`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WaveletMixOut {
    pub a: String,
    pub b: String,
    pub split: u8,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub bytes_downloaded: u64,
    pub decode_ms: u64,
    /// Set when the result was ingested at the requested `save_as`
    /// path. `None` ⇒ the bytes were returned inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_as: Option<String>,
    pub bytes_len: u64,
    pub content_type: String,
    /// Base64-encoded PNG payload. Empty when `save_as` was used.
    pub blob_base64: String,
}

/// Input for `audio_filter`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AudioFilterIn {
    /// Catalog path of the audio object.
    pub path: String,
    /// Layer indices to keep (e.g. `[0]` for bass-only). Other layers
    /// are zero-filled before the inverse Haar.
    pub keep_layers: Vec<u8>,
    #[serde(default)]
    pub save_as: Option<String>,
}

/// Output for `audio_filter`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AudioFilterOut {
    pub source: String,
    pub kept_layers: Vec<u8>,
    pub nlayers: u8,
    pub sample_rate: u32,
    pub channels: u8,
    pub bytes_downloaded: u64,
    pub decode_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_as: Option<String>,
    pub bytes_len: u64,
    pub content_type: String,
    /// Base64-encoded WAV payload. Empty when `save_as` was used.
    pub blob_base64: String,
}

// ===== Inspection tools (diff / inspect_object / inspect_shard) =========

/// Input for `diff_objects`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DiffIn {
    pub a: String,
    pub b: String,
    /// When `true`, include the full per-cell `is_common` arrays (one
    /// boolean per shard cell). Off by default to keep responses small —
    /// the per-layer counters give the same signal in a few hundred
    /// bytes instead of tens of KB.
    #[serde(default)]
    pub include_cells: bool,
}

/// One per-(channel, layer) row of `diff_objects`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DiffLayerOut {
    pub channel: u8,
    pub layer: u8,
    pub n_common: usize,
    pub n_total: usize,
    /// Only populated when `include_cells = true`. Length = `n_total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cells: Option<Vec<bool>>,
}

/// Output for `diff_objects`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DiffOut {
    pub a: String,
    pub b: String,
    pub kind: String,
    pub common: usize,
    pub total: usize,
    pub similarity_pct: f32,
    /// Approximate bytes the cluster avoided re-storing thanks to the
    /// SHA-256 shard-level dedup between these two objects.
    pub storage_saved_bytes: u64,
    pub layers: Vec<DiffLayerOut>,
}

/// Input for `inspect_object`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InspectIn {
    pub path: String,
}

/// Per-shard placement row.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InspectShardRow {
    pub idx: u32,
    pub node_idx: usize,
    pub node_addr: String,
    pub is_systematic: bool,
}

/// One (channel, layer) bucket.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InspectLayerOut {
    pub channel: u8,
    pub layer: u8,
    pub n_shards: u32,
    pub k_systematic: u16,
    pub shards: Vec<InspectShardRow>,
}

/// Output for `inspect_object`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InspectOut {
    pub path: String,
    pub kind: String,
    pub channels: u8,
    pub nlayers: u8,
    pub k: u16,
    pub layers: Vec<InspectLayerOut>,
}

/// Input for `inspect_shard`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InspectShardIn {
    pub path: String,
    pub channel: u8,
    pub layer: u8,
    pub idx: u32,
    /// When `true`, attach the (potentially large) raw payload bytes as
    /// hex. Default `false` — most callers only need the hash + node
    /// placement, and payloads can be tens of KB per shard.
    #[serde(default)]
    pub include_payload: bool,
}

/// Output for `inspect_shard`. `payload_hex` is populated only when
/// `include_payload = true` in the request.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InspectShardOut {
    pub path: String,
    pub channel: u8,
    pub layer: u8,
    pub idx: u32,
    pub is_systematic: bool,
    pub hash_hex: String,
    pub node_idx: usize,
    pub node_addr: String,
    pub sym_len: u32,
    /// K-byte coefficient vector. Systematic shards are unit vectors;
    /// non-systematic shards carry the RLNC random combination.
    pub coeffs: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hex: Option<String>,
    /// Set when the shard could not be fetched from the placement node
    /// (down, refused, hash mismatch). Mutually exclusive with the
    /// other fields above being meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
}

/// Output for `get_health` when `path` is supplied — a compact summary
/// of the underlying [`holofs_cluster::health::ObjectHealth`].
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ObjectHealthOut {
    pub object_id: u64,
    pub name: String,
    pub data_cid_hex: String,
    pub n_nodes: usize,
    pub n_live: usize,
    pub nlayers: u8,
    pub channels: u8,
    pub k: u16,
    /// Highest layer currently decodable, or `null` if no layer survives.
    pub current_resolution: Option<u8>,
}
