//! `holofs-embed` — CLIP-based image / text embeddings for semantic search.
//!
//! The gateway uses this crate to:
//!   * embed an image (decoded RGB channels from a coarse-layer DWT
//!     reconstruction) into a 512-dim vector,
//!   * embed a text query into the same space,
//!   * persist `(data_cid, layer_band) → vec` records into an
//!     append-only flat file so a restart skips already-embedded files.
//!
//! Heavy work — model weights, tokenizer, candle tensor graph — lives
//! behind [`Embedder::new`] which the gateway calls lazily on the first
//! search / first PUT after `--enable-embed`. Tests and unit-runs that
//! never search or PUT pay zero cost.
//!
//! See `/about` in `holofs-web` for the user-facing pitch; the
//! progressive-search UI on top of this crate is Stage 12.9.

#![warn(missing_docs)]

mod ann;
mod error;
mod index;
mod model;
mod text;

pub use ann::{HnswIndex, SearchHit, HNSW_MIN_BAND_SIZE};
pub use error::EmbedError;
pub use index::{EmbedRecord, Index};
pub use model::Embedder;

/// Embedding dimensionality for `openai/clip-vit-base-patch32`. Hard-coded
/// because the gateway / web layer wants a compile-time stride for the
/// flat-file index — swapping the model later means a v2 magic in
/// [`Index`] and a re-index pass.
pub const EMBED_DIM: usize = 512;

/// Layer band the embedding was computed against. Stage 12.8 only writes
/// `Coarse` (L0-L2 reconstruction at ~10% of the bytes); 12.9 / 13.3 add
/// the higher bands when the hierarchical index lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LayerBand {
    /// L0-L2 reconstruction — silhouette / colour / coarse texture.
    Coarse = 0,
    /// L0-L4 reconstruction — adds mid-frequency detail.
    Mid = 1,
    /// All layers — full resolution decode.
    Full = 2,
}

impl LayerBand {
    /// Parse from the on-disk discriminant. Returns `None` on unknown
    /// values so older / newer index files don't blow up the reader.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Coarse),
            1 => Some(Self::Mid),
            2 => Some(Self::Full),
            _ => None,
        }
    }
}
