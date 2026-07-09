//! Public error type for the Gateway API.
//!
//! Every [`Gateway`](crate::Gateway) method returns
//! `Result<_, GatewayError>`. HTTP handlers in `holofs-web`
//! map each variant to an HTTP status via
//! `holofs_web::handlers::error_to_response`:
//!
//! | Variant              | HTTP status |
//! |----------------------|-------------|
//! | `NotFound`           | 404         |
//! | `BadRequest`         | 400         |
//! | `Decode`             | 503         |
//! | `PreviewUnsupported` | 404         |
//! | `IsDirectory`        | 409         |
//! | `AlreadyExists`      | 409         |
//! | `EncodingInProgress` | 409 + Retry-After |
//! | `NotADirectory`      | 409         |
//! | `DirectoryNotEmpty`  | 409         |
//! | `ClusterDegraded`    | 503         |
//! | `Persist`            | 500         |

/// Errors the public Gateway API can return.
#[derive(Debug, Clone)]
pub enum GatewayError {
    /// Object not in catalog.
    NotFound,
    /// Client-side error (empty body, bad name, unsupported kind).
    BadRequest(String),
    /// Decode failed at the cluster level (not enough shards, network).
    Decode(String),
    /// Preview was requested for text/opaque — no graceful projection exists.
    PreviewUnsupported,
    /// Caller asked for the bytes of a `Directory` entry. Directories have
    /// no payload; the HTTP layer surfaces this as `409 Conflict`.
    IsDirectory,
    /// An entry already exists at the target path. `mkdir` returns this for
    /// any non-directory entry; PUT returns it for directory entries.
    AlreadyExists,
    /// Async ingest is currently running for this name: a previous
    /// `HOLOFS_ASYNC_ENCODE=1` PUT staged a `state=Encoding`
    /// placeholder and the background worker has not yet finished.
    /// HTTP layer surfaces this as `409 Conflict` with a
    /// `Retry-After` header so the caller polls instead of retrying
    /// immediately. Distinct from `AlreadyExists` because clients
    /// treat the two very differently: `AlreadyExists` is terminal,
    /// `EncodingInProgress` is transient.
    EncodingInProgress,
    /// `rmdir`/`list_dir` invoked on a path that exists but is not a
    /// `Directory` entry.
    NotADirectory,
    /// `rmdir` invoked on a directory that still has children. The frontend
    /// surfaces this as `409 Conflict`.
    DirectoryNotEmpty,
    /// Placement asked for a node from an empty live set — the cluster
    /// is fully down or hasn't been discovered yet. Frontends surface
    /// this as `503 Service Unavailable` so clients know to retry.
    ClusterDegraded,
    /// N4: atomic catalog save-to-disk failed. Historically
    /// `persist_catalog` swallowed IO errors with an `eprintln!` and
    /// let the caller succeed, which hid disk-full / read-only
    /// filesystem incidents until a restart discovered the truncated
    /// catalog. Now writers refuse and surface a 500 so the operator
    /// notices immediately.
    Persist(String),
    /// Async-ingest intake ceiling reached: `objects_encoding` has
    /// hit `encode_queue_max` and further 202s would let the queue
    /// grow without bound. HTTP layer surfaces this as
    /// `503 Service Unavailable` with `Retry-After` so a polling
    /// client backs off cleanly. Distinct from `ClusterDegraded`
    /// (503 without Retry-After — the cluster itself is down) and
    /// from `EncodingInProgress` (409 — one specific name is busy,
    /// not the whole gateway).
    AsyncQueueFull,
}

impl From<holofs_model::placement::NoLiveNodes> for GatewayError {
    fn from(_: holofs_model::placement::NoLiveNodes) -> Self {
        GatewayError::ClusterDegraded
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::NotFound => write!(f, "not found"),
            GatewayError::BadRequest(s) => write!(f, "bad request: {s}"),
            GatewayError::Decode(s) => write!(f, "decode: {s}"),
            GatewayError::PreviewUnsupported => write!(f, "preview not supported for this kind"),
            GatewayError::IsDirectory => write!(f, "is a directory"),
            GatewayError::AlreadyExists => write!(f, "already exists"),
            GatewayError::EncodingInProgress => {
                write!(f, "async ingest still running for this name")
            }
            GatewayError::NotADirectory => write!(f, "not a directory"),
            GatewayError::DirectoryNotEmpty => write!(f, "directory not empty"),
            GatewayError::ClusterDegraded => write!(f, "cluster has no live nodes"),
            GatewayError::Persist(s) => write!(f, "catalog persist failed: {s}"),
            GatewayError::AsyncQueueFull => {
                write!(f, "async ingest queue is full — retry after backoff")
            }
        }
    }
}

impl std::error::Error for GatewayError {}
