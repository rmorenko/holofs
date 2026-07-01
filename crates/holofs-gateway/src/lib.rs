//! holofs-gateway: catalog, decode pipeline, auto-repair, background
//! scrub, versions, GC, and semantic search — the public Gateway
//! API surface consumed by `holofs-web` HTTP handlers, `holofs-cli`
//! binaries, and `holofs-mcp` tools.
//!
//! The bulk of the implementation still lives in `http_gateway.rs`
//! for now (~4200 lines). Sibling modules host pieces that stand
//! on their own with no cross-file coupling:
//!
//! - [`error`] — `GatewayError` + `Display` + `From<NoLiveNodes>`.
//! - [`util`] — content-type sniffers, PNG encoder, `now_unix`,
//!   directory-id hasher.
//!
//! Everything re-exports at the crate root, so consumers can keep
//! using `holofs_gateway::GatewayError` etc. without following the
//! module path.

mod error;
mod http_gateway;
mod search;
mod util;
mod versions;

pub use error::GatewayError;
pub use http_gateway::*;
pub use search::{SearchBand, SemanticHit};
pub use versions::{DeleteVersionResult, RestoreResult, VersionEntry};
