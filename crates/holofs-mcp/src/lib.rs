//! MCP (Model Context Protocol) server endpoint for holofs.
//!
//! Stage 12.0 exposes a small set of **read-only** tools over the
//! Streamable HTTP transport so MCP clients (Claude Desktop, Claude
//! Code, etc.) can talk to a running holofs cluster without screen
//! scraping the web UI. The server is mounted inside the existing axum
//! router by `holofs-web`, so it shares the same `Arc<Gateway>` and the
//! same listening port.
//!
//! Tools (all read-only):
//! - `list_catalog` — enumerate entries under an optional path prefix.
//! - `read_object_text` — fetch text-kind objects as a UTF-8 string.
//! - `find_similar` — top neighbours by perceptual fingerprint / MinHash.
//! - `get_health` — cluster health index, or per-object health report.
//!
//! Write operations (`put`, `mkdir`, `rmdir`, `mv`) are intentionally
//! out of scope at this stage — see Stage 12.1.

use std::sync::Arc;

use holofs_gateway::{Gateway, SimilarScope};
use rmcp::handler::server::{
    router::tool::ToolRouter,
    wrapper::{Json, Parameters},
};
use rmcp::model::{
    Annotated, Implementation, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
    RawResource, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::{StreamableHttpServerConfig, StreamableHttpService},
};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::{Deserialize, Serialize};

/// Maximum body returned by `read_object_text`. The MCP client embeds the
/// payload directly into the model context, so a hard cap keeps a 200 MB
/// rogue object from blowing up the conversation.
const MAX_READ_TEXT_BYTES: usize = 256 * 1024;

/// MCP handler — wraps a shared `Arc<Gateway>` and a `ToolRouter`
/// dispatching to the methods below. A fresh instance is built per
/// session by the `StreamableHttpService`'s factory closure.
#[derive(Clone)]
pub struct HolofsHandler {
    gateway: Arc<Gateway>,
    /// When `false`, write tools (`put_object_text`, `mkdir`, `rmdir`,
    /// `mv_object`) reject the call with an explicit error pointing the
    /// caller at `HOLOFS_MCP_TOKEN`. Toggled by `make_mcp_service` based
    /// on whether the operator configured an auth token: no token →
    /// read-only mode for safety, so an unauthenticated `/mcp` endpoint
    /// can't be used to wipe the catalog.
    writes_enabled: bool,
    tool_router: ToolRouter<Self>,
}

impl HolofsHandler {
    /// Build a fresh handler bound to `gateway`. Cheap — only clones an
    /// `Arc` and constructs the router table.
    pub fn new(gateway: Arc<Gateway>, writes_enabled: bool) -> Self {
        Self {
            gateway,
            writes_enabled,
            tool_router: Self::tool_router(),
        }
    }

    fn require_writes(&self) -> Result<(), ErrorData> {
        if self.writes_enabled {
            Ok(())
        } else {
            Err(ErrorData::invalid_request(
                "write tools are disabled: set HOLOFS_MCP_TOKEN on the server \
                 and authenticate with `Authorization: Bearer <token>`",
                None,
            ))
        }
    }
}

// ===== Tool I/O view-models ==============================================

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
    /// `MAX_READ_TEXT_BYTES`.
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

// ===== diff_objects (Stage 12.2) ========================================

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

// ===== inspect_object / inspect_shard (Stage 12.2) =======================

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

// ===== Tool implementations ==============================================

#[tool_router]
impl HolofsHandler {
    /// List entries under a path prefix.
    #[tool(description = "List catalog entries under `prefix` (root if empty). \
                          Set `recursive=true` to walk the whole subtree.")]
    pub async fn list_catalog(
        &self,
        Parameters(ListCatalogIn { prefix, recursive }): Parameters<ListCatalogIn>,
    ) -> Result<Json<ListCatalogOut>, ErrorData> {
        let prefix = prefix.unwrap_or_default();
        let rows: Vec<CatalogRow> = if recursive {
            // Walk the catalog snapshot ourselves so we get every
            // descendant — `list_dir` is intentionally one-level deep.
            let cat = self.gateway.catalog().lock().await.clone();
            let needle = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };
            cat.entries
                .iter()
                .filter(|(k, _)| {
                    if k.as_str() == prefix {
                        return false;
                    }
                    needle.is_empty() || k.starts_with(&needle)
                })
                .map(|(k, v)| catalog_row(k, v))
                .collect()
        } else {
            self.gateway
                .list_dir(&prefix)
                .await
                .map_err(gw_err)?
                .into_iter()
                .map(|(path, m)| catalog_row(&path, &m))
                .collect::<Vec<_>>()
        };
        Ok(Json(ListCatalogOut {
            prefix,
            recursive,
            count: rows.len(),
            entries: rows,
        }))
    }

    /// Read a text-kind object as UTF-8, capped at `MAX_READ_TEXT_BYTES`.
    #[tool(description = "Fetch a text-kind catalog object as a UTF-8 string. \
                          Capped at 256 KiB; binary objects are rejected.")]
    pub async fn read_object_text(
        &self,
        Parameters(ReadTextIn { path }): Parameters<ReadTextIn>,
    ) -> Result<Json<ReadTextOut>, ErrorData> {
        use holofs_model::manifest::ObjectKind;
        let dec = self
            .gateway
            .decode_object(&path, None)
            .await
            .map_err(gw_err)?;
        if dec.kind != ObjectKind::Text {
            return Err(ErrorData::invalid_params(
                format!(
                    "object {path} is kind={:?} (not text); read_object_text refuses binary payloads",
                    dec.kind
                ),
                None,
            ));
        }
        let total = dec.bytes.len() as u64;
        let truncated = dec.bytes.len() > MAX_READ_TEXT_BYTES;
        let slice = if truncated {
            &dec.bytes[..MAX_READ_TEXT_BYTES]
        } else {
            &dec.bytes[..]
        };
        let content = String::from_utf8_lossy(slice).to_string();
        Ok(Json(ReadTextOut {
            path,
            truncated,
            total_bytes: total,
            content,
        }))
    }

    /// Top neighbours of an object by perceptual fingerprint or MinHash.
    #[tool(description = "Find the top similar objects for the given path. \
                          `scope` ∈ {all, folder, tree}; default `all`.")]
    pub async fn find_similar(
        &self,
        Parameters(SimilarIn { path, scope }): Parameters<SimilarIn>,
    ) -> Result<Json<SimilarOut>, ErrorData> {
        use holofs_gateway::SimilarityMethod;
        let scope_str = scope.unwrap_or_else(|| "all".into());
        let parsed = SimilarScope::parse(&scope_str);
        let report = self
            .gateway
            .similar_to(&path, parsed)
            .await
            .map_err(gw_err)?;
        let neighbors = report
            .neighbors
            .into_iter()
            .map(|m| SimilarRow {
                path: m.name,
                similarity_pct: m.similarity_pct,
                method: match m.method {
                    SimilarityMethod::Jaccard => "Jaccard".into(),
                    SimilarityMethod::DHash => "DHash".into(),
                },
            })
            .collect();
        Ok(Json(SimilarOut {
            target: report.name,
            kind: kind_str(report.kind),
            scope: scope_str,
            neighbors,
        }))
    }

    /// Cluster-wide health index — live/total node counts and the
    /// catalog snapshot.
    #[tool(description = "Cluster health index: live vs total nodes (admin-kill aware), \
                          per-node addresses + zones, sorted catalog names.")]
    pub async fn get_cluster_health(&self) -> Json<HealthIndexOut> {
        let idx = self.gateway.health_index_data().await;
        let nodes: Vec<NodeRow> = idx
            .nodes
            .iter()
            .map(|n| NodeRow {
                idx: n.idx,
                addr: n.addr.clone(),
                zone: n.zone,
                admin_killed: n.admin_killed,
            })
            .collect();
        Json(HealthIndexOut {
            n_live: idx.n_live,
            n_total: idx.n_total,
            nodes,
            objects: idx.objects,
        })
    }

    /// Per-object decode-readiness summary.
    #[tool(description = "Per-object health summary: how many layers survive, current decodable \
                          resolution, total/live placement node counts.")]
    pub async fn get_object_health(
        &self,
        Parameters(ObjectHealthIn { path }): Parameters<ObjectHealthIn>,
    ) -> Result<Json<ObjectHealthOut>, ErrorData> {
        use holofs_core::hash::hex;
        let oh = self.gateway.object_health(&path).await.map_err(gw_err)?;
        Ok(Json(ObjectHealthOut {
            object_id: oh.object_id,
            name: oh.name,
            data_cid_hex: hex(&oh.data_cid),
            n_nodes: oh.n_nodes,
            n_live: oh.n_live,
            nlayers: oh.nlayers,
            channels: oh.channels,
            k: oh.k,
            current_resolution: oh.current_resolution,
        }))
    }

    // ===== Inspection tools (Stage 12.2) =================================

    /// Per-(channel, layer) chunk-level diff between two same-kind
    /// objects. Mirrors the `/diff?a=…&b=…` page.
    #[tool(description = "Byte-perfect chunk diff between two same-kind objects: per-layer \
                          common counts, similarity %, storage saved by dedup. Set \
                          `include_cells=true` for the full per-shard boolean grid.")]
    pub async fn diff_objects(
        &self,
        Parameters(DiffIn {
            a,
            b,
            include_cells,
        }): Parameters<DiffIn>,
    ) -> Result<Json<DiffOut>, ErrorData> {
        let report = self.gateway.diff_chunks(&a, &b).await.map_err(gw_err)?;
        let layers = report
            .layers
            .into_iter()
            .map(|l| DiffLayerOut {
                channel: l.channel,
                layer: l.layer,
                n_common: l.n_common,
                n_total: l.n_total,
                cells: include_cells.then(|| l.cells.iter().map(|c| c.is_common).collect()),
            })
            .collect();
        Ok(Json(DiffOut {
            a: report.name_a,
            b: report.name_b,
            kind: kind_str(report.kind),
            common: report.common,
            total: report.total,
            similarity_pct: report.similarity_pct,
            storage_saved_bytes: report.storage_saved_bytes,
            layers,
        }))
    }

    /// Per-(channel, layer) shard layout for one object — node placement
    /// and systematic vs RLNC marker for every shard.
    #[tool(description = "Per-(channel, layer) shard layout for `path`: which node holds \
                          each shard, which shards are systematic vs RLNC. Useful for \
                          reasoning about placement and replica-loss scenarios.")]
    pub async fn inspect_object(
        &self,
        Parameters(InspectIn { path }): Parameters<InspectIn>,
    ) -> Result<Json<InspectOut>, ErrorData> {
        let info = self.gateway.inspect(&path).await.map_err(gw_err)?;
        let layers = info
            .layers
            .into_iter()
            .map(|l| InspectLayerOut {
                channel: l.channel,
                layer: l.layer,
                n_shards: l.n_shards,
                k_systematic: l.k_systematic,
                shards: l
                    .shards
                    .into_iter()
                    .map(|s| InspectShardRow {
                        idx: s.idx,
                        node_idx: s.node_idx,
                        node_addr: s.node_addr,
                        is_systematic: s.is_systematic,
                    })
                    .collect(),
            })
            .collect();
        Ok(Json(InspectOut {
            path: info.name,
            kind: kind_str(info.kind),
            channels: info.channels,
            nlayers: info.nlayers,
            k: info.k,
            layers,
        }))
    }

    /// Fetch one specific shard's metadata (and optionally its payload).
    /// Re-runs HRW placement + the SHA-256 integrity check via the
    /// gateway, so the result reflects current cluster liveness.
    #[tool(description = "Fetch one shard's metadata from its placement node: hash, coeff \
                          vector, systematic flag, node addr. Pass `include_payload=true` \
                          to also get the raw bytes as hex.")]
    pub async fn inspect_shard(
        &self,
        Parameters(InspectShardIn {
            path,
            channel,
            layer,
            idx,
            include_payload,
        }): Parameters<InspectShardIn>,
    ) -> Result<Json<InspectShardOut>, ErrorData> {
        match self
            .gateway
            .shard_payload(&path, channel, layer, idx)
            .await
            .map_err(gw_err)?
        {
            Some(sp) => Ok(Json(InspectShardOut {
                path,
                channel,
                layer,
                idx,
                is_systematic: sp.is_systematic,
                hash_hex: sp.hash_hex,
                node_idx: sp.node_idx,
                node_addr: sp.node_addr,
                sym_len: sp.sym_len,
                coeffs: sp.coeffs,
                payload_hex: include_payload.then(|| hex_encode(&sp.payload)),
                fetch_error: None,
            })),
            // `None` means the node was unreachable / returned a bad
            // hash. Surface that explicitly so the caller can correlate
            // with health reports instead of guessing.
            None => Ok(Json(InspectShardOut {
                path,
                channel,
                layer,
                idx,
                is_systematic: false,
                hash_hex: String::new(),
                node_idx: 0,
                node_addr: String::new(),
                sym_len: 0,
                coeffs: Vec::new(),
                payload_hex: None,
                fetch_error: Some("shard not retrievable from placement node".into()),
            })),
        }
    }

    // ===== Write tools (Stage 12.1) ======================================
    // All four require the operator-side `HOLOFS_MCP_TOKEN` to be set —
    // see `HolofsHandler::require_writes`. Without that the calls return
    // an explicit `invalid_request` so the client knows it's a config
    // issue, not a transient failure.

    /// Write or overwrite a text-kind catalog object.
    #[tool(description = "Write a UTF-8 string to a catalog path. Parent directory must \
                          exist (call `mkdir` first). Requires the server's \
                          HOLOFS_MCP_TOKEN to be configured.")]
    pub async fn put_object_text(
        &self,
        Parameters(PutTextIn {
            path,
            content,
            content_type,
        }): Parameters<PutTextIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        self.require_writes()?;
        if content.is_empty() {
            return Err(ErrorData::invalid_params("content is empty", None));
        }
        // `ingest_bytes` chooses kind / MIME from the bytes themselves; we
        // pass the body verbatim. Custom `content_type` is recorded as a
        // hint in the response since the gateway sniffs the MIME on its
        // own — we don't override it from MCP for safety (a wrong MIME
        // would break later decodes).
        let res = self
            .gateway
            .ingest_bytes(&path, content.as_bytes())
            .await
            .map_err(gw_err)?;
        let note = match content_type {
            Some(ct) => Some(format!(
                "ingested {} bytes, {} shards (requested content_type='{ct}' ignored — \
                 gateway sniffs MIME from payload)",
                content.len(),
                res.total_shards
            )),
            None => Some(format!(
                "ingested {} bytes, {} shards",
                content.len(),
                res.total_shards
            )),
        };
        Ok(Json(WriteOk {
            path: res.name,
            action: "wrote".into(),
            note,
        }))
    }

    /// Create a directory at `path`. Parent must already exist.
    #[tool(description = "Create a directory at `path`. The parent directory must already \
                          exist. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn mkdir(
        &self,
        Parameters(DirIn { path }): Parameters<DirIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        self.require_writes()?;
        let _ = self.gateway.mkdir(&path).await.map_err(gw_err)?;
        Ok(Json(WriteOk {
            path,
            action: "created".into(),
            note: None,
        }))
    }

    /// Remove an *empty* directory.
    #[tool(description = "Remove an empty directory. Non-empty directories are rejected — \
                          delete the children first. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn rmdir(
        &self,
        Parameters(DirIn { path }): Parameters<DirIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        self.require_writes()?;
        let _ = self.gateway.rmdir(&path).await.map_err(gw_err)?;
        Ok(Json(WriteOk {
            path,
            action: "removed".into(),
            note: None,
        }))
    }

    /// Rename / move a catalog object to a new path.
    #[tool(description = "Rename a catalog object from `from` to `to`. Works on both \
                          objects and directories. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn mv_object(
        &self,
        Parameters(MoveIn { from, to }): Parameters<MoveIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        self.require_writes()?;
        let _ = self.gateway.rename(&from, &to).await.map_err(gw_err)?;
        Ok(Json(WriteOk {
            path: to.clone(),
            action: "renamed".into(),
            note: Some(format!("from {from}")),
        }))
    }
}

// ===== ServerHandler ======================================================

#[tool_handler]
impl ServerHandler for HolofsHandler {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        // Stage 12.3: also advertise `resources/` — every catalog entry
        // is addressable via a `holofs:///path` URI and can be read as
        // text (for text-kind) or as a base64 blob (image / audio /
        // opaque). Directories are listed but not readable; the client
        // is expected to walk down via prefix.
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        let mut imp = Implementation::default();
        imp.name = "holofs".into();
        imp.version = env!("CARGO_PKG_VERSION").into();
        info.server_info = imp;
        let mode = if self.writes_enabled {
            "read+write"
        } else {
            "read-only (set HOLOFS_MCP_TOKEN on the server to enable writes)"
        };
        info.instructions = Some(format!(
            "MCP tools for a running holofs cluster, mode: {mode}.\n\
             Read tools: list_catalog, read_object_text, find_similar, \
             get_cluster_health, get_object_health, diff_objects, \
             inspect_object, inspect_shard.\n\
             Write tools (gated by HOLOFS_MCP_TOKEN): put_object_text, mkdir, \
             rmdir, mv_object.\n\
             Resources: every catalog entry at `holofs:///<path>` (text → \
             utf-8, binary → base64 blob; truncated to 1 MiB).\n\
             Catalog paths use forward-slash directories \
             (e.g. `photos/2026/img.png`)."
        ));
        info
    }

    // ===== Resources (Stage 12.3) ========================================
    // Catalog entries are exposed as MCP resources at
    // `holofs:///<path>`. `list_resources` enumerates files (directories
    // are skipped — they don't have readable content of their own);
    // `read_resource` decodes the object on demand and returns either a
    // text or base64 blob depending on the kind.

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        use holofs_model::manifest::ObjectKind;
        let cat = self.gateway.catalog().lock().await.clone();
        let resources = cat
            .entries
            .iter()
            .filter(|(_, m)| m.kind != ObjectKind::Directory)
            .map(|(name, m)| {
                let n_shards: usize = m
                    .shard_hashes
                    .iter()
                    .flat_map(|c| c.iter())
                    .map(|l| l.len())
                    .sum();
                let r = RawResource::new(format!("holofs:///{name}"), name.clone())
                    .with_mime_type(m.content_type.clone())
                    .with_description(format!(
                        "kind={}, shards={n_shards}",
                        kind_str(m.kind)
                    ));
                Annotated::new(r, None)
            })
            .collect();
        let mut out = ListResourcesResult::default();
        out.resources = resources;
        Ok(out)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use holofs_model::manifest::ObjectKind;

        let uri = request.uri;
        let name = parse_holofs_uri(&uri).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("unrecognised resource URI: {uri:?} (expected holofs:///<path>)"),
                None,
            )
        })?;
        let dec = self.gateway.decode_object(&name, None).await.map_err(gw_err)?;
        if dec.kind == ObjectKind::Directory {
            return Err(ErrorData::invalid_params(
                "directories are not readable resources; list children via list_catalog",
                None,
            ));
        }

        // Cap reads at 1 MiB so a single resource fetch can't blow up
        // the client's context window. Text objects keep their UTF-8
        // semantics; everything else becomes a base64 blob whose
        // mime_type is taken straight from the manifest.
        const MAX_BYTES: usize = 1024 * 1024;
        let bytes = if dec.bytes.len() > MAX_BYTES {
            &dec.bytes[..MAX_BYTES]
        } else {
            &dec.bytes[..]
        };
        let contents = match dec.kind {
            ObjectKind::Text => {
                let mut c = ResourceContents::text(
                    String::from_utf8_lossy(bytes).into_owned(),
                    uri.clone(),
                );
                c = c.with_mime_type(dec.content_type.clone());
                c
            }
            _ => {
                let b64 = STANDARD.encode(bytes);
                let mut c = ResourceContents::blob(b64, uri.clone());
                c = c.with_mime_type(dec.content_type.clone());
                c
            }
        };
        Ok(ReadResourceResult::new(vec![contents]))
    }
}

// ===== Service factory ====================================================

/// Build the `StreamableHttpService` tower service ready to mount under
/// any axum / hyper router. Each MCP session gets a fresh
/// `HolofsHandler` (cheap — just an `Arc` clone), and they all share the
/// same underlying `Gateway`.
///
/// `writes_enabled` should mirror "the operator configured an auth token
/// on the transport": if `false`, write tools refuse to run with an
/// explicit error so a misconfigured deployment can't be used to mutate
/// the catalog. The caller (typically `holofs-web`) flips this to
/// `true` once it has the bearer-token middleware active in front of
/// the service.
pub fn make_mcp_service(
    gateway: Arc<Gateway>,
    writes_enabled: bool,
) -> StreamableHttpService<HolofsHandler, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(HolofsHandler::new(Arc::clone(&gateway), writes_enabled)),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

// ===== helpers ============================================================

fn catalog_row(path: &str, m: &holofs_model::manifest::Manifest) -> CatalogRow {
    // Total systematic-shard count across (channel, layer) buckets.
    let n_shards: u32 = m
        .shard_hashes
        .iter()
        .flat_map(|chan| chan.iter())
        .map(|layer| layer.len() as u32)
        .sum();
    CatalogRow {
        path: path.to_string(),
        kind: kind_str(m.kind),
        content_type: m.content_type.clone(),
        n_shards,
        width: m.width,
        height: m.height,
        audio_sample_rate: m.audio_sample_rate,
        created_at_unix: m.created_at_unix,
    }
}

fn kind_str(k: holofs_model::manifest::ObjectKind) -> String {
    use holofs_model::manifest::ObjectKind;
    match k {
        ObjectKind::Image => "image",
        ObjectKind::Audio => "audio",
        ObjectKind::Text => "text",
        ObjectKind::Opaque => "opaque",
        ObjectKind::Directory => "directory",
    }
    .to_string()
}

/// Strip the `holofs:///` scheme + authority off a resource URI and
/// return the catalog path. Both `holofs:///path` (canonical) and
/// `holofs://path` (degenerate) are accepted; anything else returns
/// `None`. Empty path is also rejected — the catalog root isn't a
/// readable resource.
fn parse_holofs_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("holofs:///").or_else(|| uri.strip_prefix("holofs://"))?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Lowercase hex encoding. Used for shard-payload bytes when the
/// caller opts into `include_payload`. Kept local instead of pulling in
/// the `hex` crate just for one call site.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn gw_err(e: holofs_gateway::GatewayError) -> ErrorData {
    use holofs_gateway::GatewayError;
    match e {
        GatewayError::NotFound => ErrorData::invalid_params("object not found", None),
        GatewayError::NotADirectory => {
            ErrorData::invalid_params("path is not a directory", None)
        }
        GatewayError::BadRequest(s) => ErrorData::invalid_params(s, None),
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_holofs_uri() {
        assert_eq!(
            parse_holofs_uri("holofs:///photos/2026/img.png"),
            Some("photos/2026/img.png".into())
        );
        assert_eq!(parse_holofs_uri("holofs:///note.txt"), Some("note.txt".into()));
    }

    #[test]
    fn parses_degenerate_two_slash_form() {
        // Some clients emit `holofs://path` (no triple slash). Accept it
        // so a hand-typed URI still resolves.
        assert_eq!(parse_holofs_uri("holofs://note.txt"), Some("note.txt".into()));
    }

    #[test]
    fn rejects_other_schemes_and_empty_path() {
        assert_eq!(parse_holofs_uri("file:///etc/passwd"), None);
        assert_eq!(parse_holofs_uri("https://example.com"), None);
        assert_eq!(parse_holofs_uri("holofs:///"), None);
        assert_eq!(parse_holofs_uri(""), None);
    }
}
