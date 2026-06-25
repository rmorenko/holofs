//! Tiny shim around the CLIP tokenizer.
//!
//! `tokenizers::Tokenizer` already does all the heavy lifting; we just
//! produce a `Vec<u32>` of `input_ids` padded / truncated to CLIP's
//! 77-token context. Padding token id is fixed at 0 in the original
//! CLIP vocab — matches `pad_token_id` from the HF config.

use crate::error::EmbedError;

const MAX_TOKENS: usize = 77;
const PAD_TOKEN_ID: u32 = 0;

/// Tokenise `text` into the 77-long u32 sequence CLIP's text encoder
/// expects. Padding is right-padding with zero, truncation drops the
/// tail (mirrors HF's `model_max_length=77, truncation=True` default).
pub(crate) fn tokenize_for_clip(
    tokenizer: &tokenizers::Tokenizer,
    text: &str,
) -> Result<Vec<u32>, EmbedError> {
    let enc = tokenizer
        .encode(text, true)
        .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
    let mut ids = enc.get_ids().to_vec();
    if ids.len() > MAX_TOKENS {
        ids.truncate(MAX_TOKENS);
    } else {
        ids.resize(MAX_TOKENS, PAD_TOKEN_ID);
    }
    Ok(ids)
}
