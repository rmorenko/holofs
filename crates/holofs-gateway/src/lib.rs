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

mod diff;
mod error;
mod escrow;
mod gc;
mod http_gateway;
mod inspect;
mod mix;
mod search;
mod similarity;
mod spotlight;
mod util;
mod versions;

pub use diff::{DiffCell, DiffLayer, DiffReport};
pub use error::GatewayError;
pub use escrow::{
    EscrowRecoverResult, EscrowShareBytes, EscrowShareInfo, EscrowSplitResult,
};
pub use gc::{GcNodeReport, GcReport};
pub use http_gateway::*;
pub use inspect::{InspectInfo, LayerLayout, ShardInfo, ShardPayload};
pub use mix::{FilteredAudio, MixedImage};
pub use search::{SearchBand, SemanticHit};
pub use similarity::{
    ShardOverlap, SimilarMatch, SimilarReport, SimilarScope, SimilarityMethod,
};
pub use spotlight::{SpotlightImage, SpotlightRoi};
pub use versions::{DeleteVersionResult, RestoreResult, VersionEntry};
