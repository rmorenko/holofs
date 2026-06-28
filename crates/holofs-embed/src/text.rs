//! Tokeniser shim for the multilingual text encoder
//! (`sentence-transformers/clip-ViT-B-32-multilingual-v1` —
//! distilbert-base-multilingual-cased + a linear projection
//! into the CLIP image-embedding space).
//!
//! `tokenizers::Tokenizer` does the heavy lifting; the helper here
//! just frames `text` with `[CLS] … [SEP]` (BERT convention) and
//! returns both the `input_ids` and the matching attention mask so
//! the encoder can mean-pool over real tokens only — padded slots
//! must be excluded.
//!
//! Truncation: DistilBERT's `max_position_embeddings = 512`, but
//! image search queries are short by nature. We cap at 64 — enough
//! for a long descriptive query in any language without paying for
//! attention over hundreds of zero-padded slots.

use crate::error::EmbedError;

/// Cap query length at 64 tokens. CLIP-side was 77; DistilBERT can
/// handle 512 but a search query never gets near that and the
/// quadratic attention cost over zero padding is wasted work.
pub(crate) const MAX_TOKENS: usize = 64;

/// `[PAD]` id in `distilbert-base-multilingual-cased`. Stable
/// across vocab releases; same value used by every BERT
/// derivative.
const PAD_TOKEN_ID: u32 = 0;

/// Tokenise `text` for the multilingual DistilBERT text encoder.
///
/// Returns `(input_ids, attention_mask)` both padded / truncated to
/// [`MAX_TOKENS`]. `attention_mask[i] = 1` for real tokens (including
/// the `[CLS]` / `[SEP]` framing tokens) and `0` for the right-padded
/// `[PAD]` tail — the mean-pool inside the encoder reads this so
/// only real tokens contribute to the pooled vector.
pub(crate) fn tokenize_for_distilbert(
    tokenizer: &tokenizers::Tokenizer,
    text: &str,
) -> Result<(Vec<u32>, Vec<u32>), EmbedError> {
    // `add_special_tokens=true` makes the tokenizer wrap the input
    // with `[CLS] … [SEP]` per the BERT convention configured in the
    // upstream `tokenizer.json`.
    let enc = tokenizer
        .encode(text, true)
        .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
    let mut ids = enc.get_ids().to_vec();
    let mut mask = enc.get_attention_mask().to_vec();
    if ids.len() > MAX_TOKENS {
        ids.truncate(MAX_TOKENS);
        mask.truncate(MAX_TOKENS);
        // Force the truncated tail to end on `[SEP]` so the pooled
        // vector reflects a properly-terminated sequence. The id of
        // `[SEP]` is 102 across every BERT-multilingual vocab; we
        // pull it from the tokenizer to stay vocab-agnostic.
        if let Some(sep_id) = tokenizer.token_to_id("[SEP]") {
            if let Some(last) = ids.last_mut() {
                *last = sep_id;
            }
        }
    } else {
        ids.resize(MAX_TOKENS, PAD_TOKEN_ID);
        mask.resize(MAX_TOKENS, 0);
    }
    Ok((ids, mask))
}
