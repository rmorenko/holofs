//! MCP (Model Context Protocol) server endpoint for holofs.
//!
//! Stage 12.0 exposes a small set of tools over the Streamable HTTP
//! transport so MCP clients (Claude Desktop, Claude Code, etc.) can
//! talk to a running holofs cluster without screen scraping the web
//! UI. The server is mounted inside the existing axum router by
//! `holofs-web`, so it shares the same `Arc<Gateway>` and the same
//! listening port.
//!
//! Tool families:
//! - **Read** — `list_catalog`, `read_object_text`, `find_similar`,
//!   `get_cluster_health`, `get_object_health`, `diff_objects`,
//!   `inspect_object`, `inspect_shard`.
//! - **Transform** — `wavelet_mix`, `audio_filter`. Return bytes inline
//!   or (when `save_as` is set) ingest to the catalog. Save path
//!   requires `HOLOFS_MCP_TOKEN`.
//! - **Write** — `put_object_text`, `mkdir`, `rmdir`, `mv_object`.
//!   Require `HOLOFS_MCP_TOKEN`.
//!
//! ## Layout
//!
//! The rmcp `#[tool_router]` / `#[tool_handler]` macros generate code
//! that expects to see all tools declared inside a single `impl`
//! block. To keep this file readable, every tool method here is a
//! thin dispatch that delegates to a free function in a sibling
//! module:
//!
//! - [`views`] — MCP tool I/O DTOs.
//! - [`util`] — helpers shared by tool bodies (`gw_err`, `kind_str`,
//!   `wrap_artifact_result`, etc.).
//! - [`tools_read`] / [`tools_write`] / [`tools_transform`] — tool
//!   bodies grouped by domain.
//! - [`resources`] — MCP `resources/list` + `resources/read` bodies.

pub mod resources;
pub mod tools_read;
pub mod tools_transform;
pub mod tools_write;
pub mod util;
pub mod views;

use std::sync::Arc;

use holofs_gateway::Gateway;
use rmcp::handler::server::{
    router::tool::ToolRouter,
    wrapper::{Json, Parameters},
};
use rmcp::model::{
    Implementation, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResult, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::{StreamableHttpServerConfig, StreamableHttpService},
};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use crate::views::{
    AudioFilterIn, AudioFilterOut, DiffIn, DiffOut, DirIn, HealthIndexOut, InspectIn, InspectOut,
    InspectShardIn, InspectShardOut, ListCatalogIn, ListCatalogOut, MoveIn, ObjectHealthIn,
    ObjectHealthOut, PutTextIn, ReadTextIn, ReadTextOut, SimilarIn, SimilarOut, WaveletMixIn,
    WaveletMixOut, WriteOk,
};

/// MCP handler — wraps a shared `Arc<Gateway>` and a `ToolRouter`
/// dispatching to the methods below. A fresh instance is built per
/// session by the `StreamableHttpService`'s factory closure.
#[derive(Clone)]
pub struct HolofsHandler {
    pub(crate) gateway: Arc<Gateway>,
    /// When `false`, write tools (`put_object_text`, `mkdir`, `rmdir`,
    /// `mv_object`) reject the call with an explicit error pointing the
    /// caller at `HOLOFS_MCP_TOKEN`. Toggled by `make_mcp_service` based
    /// on whether the operator configured an auth token: no token →
    /// read-only mode for safety, so an unauthenticated `/mcp` endpoint
    /// can't be used to wipe the catalog.
    pub(crate) writes_enabled: bool,
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

    pub(crate) fn require_writes(&self) -> Result<(), ErrorData> {
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

// ===== Tool dispatch ======================================================
//
// Each `#[tool]` method here is a two-line delegation into a free
// function in the matching sibling module. Keeping the descriptions +
// method signatures colocated with the router keeps the MCP schema
// generation happy while pushing the actual bodies to files organised
// by domain.

#[tool_router]
impl HolofsHandler {
    #[tool(description = "List catalog entries under `prefix` (root if empty). \
                          Set `recursive=true` to walk the whole subtree.")]
    pub async fn list_catalog(
        &self,
        Parameters(args): Parameters<ListCatalogIn>,
    ) -> Result<Json<ListCatalogOut>, ErrorData> {
        tools_read::list_catalog(self, args).await.map(Json)
    }

    #[tool(description = "Fetch a text-kind catalog object as a UTF-8 string. \
                          Capped at 256 KiB; binary objects are rejected.")]
    pub async fn read_object_text(
        &self,
        Parameters(args): Parameters<ReadTextIn>,
    ) -> Result<Json<ReadTextOut>, ErrorData> {
        tools_read::read_object_text(self, args).await.map(Json)
    }

    #[tool(description = "Find the top similar objects for the given path. \
                          `scope` ∈ {all, folder, tree}; default `all`.")]
    pub async fn find_similar(
        &self,
        Parameters(args): Parameters<SimilarIn>,
    ) -> Result<Json<SimilarOut>, ErrorData> {
        tools_read::find_similar(self, args).await.map(Json)
    }

    #[tool(description = "Cluster health index: live vs total nodes (admin-kill aware), \
                          per-node addresses + zones, sorted catalog names.")]
    pub async fn get_cluster_health(&self) -> Json<HealthIndexOut> {
        Json(tools_read::get_cluster_health(self).await)
    }

    #[tool(description = "Per-object health summary: how many layers survive, current decodable \
                          resolution, total/live placement node counts.")]
    pub async fn get_object_health(
        &self,
        Parameters(args): Parameters<ObjectHealthIn>,
    ) -> Result<Json<ObjectHealthOut>, ErrorData> {
        tools_read::get_object_health(self, args).await.map(Json)
    }

    #[tool(description = "Byte-perfect chunk diff between two same-kind objects: per-layer \
                          common counts, similarity %, storage saved by dedup. Set \
                          `include_cells=true` for the full per-shard boolean grid.")]
    pub async fn diff_objects(
        &self,
        Parameters(args): Parameters<DiffIn>,
    ) -> Result<Json<DiffOut>, ErrorData> {
        tools_read::diff_objects(self, args).await.map(Json)
    }

    #[tool(description = "Per-(channel, layer) shard layout for `path`: which node holds \
                          each shard, which shards are systematic vs RLNC. Useful for \
                          reasoning about placement and replica-loss scenarios.")]
    pub async fn inspect_object(
        &self,
        Parameters(args): Parameters<InspectIn>,
    ) -> Result<Json<InspectOut>, ErrorData> {
        tools_read::inspect_object(self, args).await.map(Json)
    }

    #[tool(description = "Fetch one shard's metadata from its placement node: hash, coeff \
                          vector, systematic flag, node addr. Pass `include_payload=true` \
                          to also get the raw bytes as hex.")]
    pub async fn inspect_shard(
        &self,
        Parameters(args): Parameters<InspectShardIn>,
    ) -> Result<Json<InspectShardOut>, ErrorData> {
        tools_read::inspect_shard(self, args).await.map(Json)
    }

    #[tool(description = "Write a UTF-8 string to a catalog path. Parent directory must \
                          exist (call `mkdir` first). Requires the server's \
                          HOLOFS_MCP_TOKEN to be configured.")]
    pub async fn put_object_text(
        &self,
        Parameters(args): Parameters<PutTextIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        tools_write::put_object_text(self, args).await.map(Json)
    }

    #[tool(description = "Create a directory at `path`. The parent directory must already \
                          exist. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn mkdir(
        &self,
        Parameters(args): Parameters<DirIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        tools_write::mkdir(self, args).await.map(Json)
    }

    #[tool(description = "Remove an empty directory. Non-empty directories are rejected — \
                          delete the children first. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn rmdir(
        &self,
        Parameters(args): Parameters<DirIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        tools_write::rmdir(self, args).await.map(Json)
    }

    #[tool(description = "Rename a catalog object from `from` to `to`. Works on both \
                          objects and directories. Requires HOLOFS_MCP_TOKEN.")]
    pub async fn mv_object(
        &self,
        Parameters(args): Parameters<MoveIn>,
    ) -> Result<Json<WriteOk>, ErrorData> {
        tools_write::mv_object(self, args).await.map(Json)
    }

    #[tool(description = "Combine two compatible images at a DWT split layer. Layers 0..=split \
                          come from `a`, layers above from `b`. Result is a PNG. Pass \
                          `save_as` to ingest it as a new catalog object; otherwise the bytes \
                          are returned base64-encoded.")]
    pub async fn wavelet_mix(
        &self,
        Parameters(args): Parameters<WaveletMixIn>,
    ) -> Result<Json<WaveletMixOut>, ErrorData> {
        tools_transform::wavelet_mix(self, args).await
    }

    #[tool(description = "Render an audio object with only the listed layers contributing — \
                          everything else is zero-filled before the inverse Haar (selective \
                          frequency-band cut). L0 is the bass envelope, the highest layer is \
                          treble. Result is a WAV. Pass `save_as` to ingest as a new catalog \
                          object.")]
    pub async fn audio_filter(
        &self,
        Parameters(args): Parameters<AudioFilterIn>,
    ) -> Result<Json<AudioFilterOut>, ErrorData> {
        tools_transform::audio_filter(self, args).await
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
             Transform tools (return bytes inline or save to catalog via \
             save_as; writes gated by HOLOFS_MCP_TOKEN): wavelet_mix, \
             audio_filter.\n\
             Write tools (gated by HOLOFS_MCP_TOKEN): put_object_text, mkdir, \
             rmdir, mv_object.\n\
             Resources: every catalog entry at `holofs:///<path>` (text → \
             utf-8, binary → base64 blob; truncated to 1 MiB).\n\
             Catalog paths use forward-slash directories \
             (e.g. `photos/2026/img.png`)."
        ));
        info
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        resources::list_resources(self, request).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        resources::read_resource(self, request).await
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
