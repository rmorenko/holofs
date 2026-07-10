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

mod decode;
mod diff;
mod dirops;
mod error;
mod escrow;
mod fingerprint;
mod gc;
mod health;
mod http_gateway;
mod ingest;
mod inspect;
mod metrics;
mod mix;
mod repair;
mod search;
mod similarity;
mod spotlight;
pub mod util;
mod versions;

pub use decode::DecodedObject;
pub use diff::{DiffCell, DiffLayer, DiffReport};
pub use dirops::{MkdirResult, RemoveResult, RenameResult, RmdirResult};
pub use error::GatewayError;
pub use escrow::{
    EscrowRecoverResult, EscrowShareBytes, EscrowShareInfo, EscrowSplitResult,
};
pub use fingerprint::FingerprintInfo;
pub use gc::{GcNodeReport, GcReport};
pub use health::{
    AdminToggleResult, ApiStats, HealthIndexData, KindCounts, NodeStatus, ScrubReport,
};
pub use http_gateway::*;
pub use ingest::{IngestOutcome, IngestResult};
pub use inspect::{InspectInfo, LayerLayout, ShardInfo, ShardPayload};
pub use metrics::{AudioBandEnergy, FileMetrics, NeighbourMetric};
pub use mix::{FilteredAudio, MixedImage};
pub use search::{SearchBand, SemanticHit};
pub use similarity::{
    ShardOverlap, SimilarMatch, SimilarReport, SimilarScope, SimilarityMethod,
};
pub use spotlight::{SpotlightImage, SpotlightRoi};
pub use versions::{DeleteVersionResult, RestoreResult, VersionEntry};
