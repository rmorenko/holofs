//! Error type for the embed crate. Thin wrapper around the underlying
//! candle / tokenizer / IO errors with a `BadInput` arm for "you handed
//! me garbage", separate from "the model exploded".

use thiserror::Error;

/// Anything that can go wrong inside `holofs-embed`. Most call sites
/// surface this back to the gateway as a 500-ish, except `BadInput`
/// which the caller can map to a 400.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// IO failure when reading the index file or model cache.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// candle tensor failure — out-of-memory, shape mismatch, etc.
    #[error("candle: {0}")]
    Candle(String),
    /// hf-hub failure (network, auth, disk space).
    #[error("hf-hub: {0}")]
    HfHub(String),
    /// Tokenizer failure (bad UTF-8, unknown token).
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    /// Caller handed us malformed input (wrong dimensions, etc.).
    #[error("bad input: {0}")]
    BadInput(String),
    /// On-disk index file is corrupt or has an unknown magic.
    #[error("index corrupt: {0}")]
    Corrupt(String),
}

impl From<candle_core::Error> for EmbedError {
    fn from(e: candle_core::Error) -> Self {
        Self::Candle(e.to_string())
    }
}
