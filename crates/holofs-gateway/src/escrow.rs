//! holographic key escrow (threshold RLNC erasure — NOT Shamir; see
//! `crates/holofs-analytics/src/escrow.rs` §Security for why).
//!
//! `escrow_split` chops a file into `n` RLNC shares, any `k` of
//! which can reconstruct the original. Shares are held in a
//! process-local `escrow_cache` map keyed by `escrow_id_hex` and
//! served on demand at `/escrow/download/<id>_<idx>.holoshare`.
//! `.holoshare` files are **not stored on the cluster** — they
//! live only in gateway RAM until download or restart.
//!

use holofs_core::hash::hex;

use crate::error::GatewayError;
use crate::util::guess_opaque_content_type;
use crate::Gateway;

/// One row of [`EscrowSplitResult::shares`] — describes a `.holoshare` file
/// the frontend should offer as a download link.
#[derive(Debug, Clone)]
pub struct EscrowShareInfo {
    /// 0-based share index inside the `n`-share set.
    pub idx: u16,
    /// `<id_hex>_<idx>.holoshare` URL stem; full URL is
    /// `/escrow/download/{download_path}`.
    pub download_path: String,
    /// Human-readable filename to offer to the user agent.
    pub filename: String,
    /// Size of the encoded `.holoshare` blob in bytes.
    pub bytes: usize,
}

/// Result of [`Gateway::escrow_split`].
#[derive(Debug, Clone)]
pub struct EscrowSplitResult {
    /// Original filename submitted by the user.
    pub filename: String,
    /// Raw input size in bytes.
    pub source_bytes: usize,
    /// Threshold (`k` of `n` needed to recover).
    pub k: usize,
    /// Total share count produced.
    pub n: usize,
    /// 32-char hex of `escrow_id` (first 16 bytes of SHA-256 over the source).
    pub escrow_id_hex: String,
    /// One entry per produced share, in index order.
    pub shares: Vec<EscrowShareInfo>,
}

/// Result of [`Gateway::escrow_download`] — encoded `.holoshare` bytes.
#[derive(Debug, Clone)]
pub struct EscrowShareBytes {
    /// 0-based share index inside the `n`-share set.
    pub idx: u16,
    /// `total_n` from the share metadata (for the `Content-Disposition` filename).
    pub total_n: u16,
    /// Encoded `.holoshare` payload.
    pub bytes: Vec<u8>,
}

/// Result of [`Gateway::escrow_recover`] — recovered file payload + metadata.
#[derive(Debug, Clone)]
pub struct EscrowRecoverResult {
    /// Decoded file bytes.
    pub data: Vec<u8>,
    /// MIME type the source was uploaded with (from the first share).
    pub content_type: String,
    /// Original filename for `Content-Disposition`.
    pub filename: String,
    /// Number of valid `.holoshare` files consumed.
    pub shares_used: usize,
}

impl Gateway {
    /// Split a file into `n` `.holoshare` shares with threshold `k`. Stores
    /// the in-memory share set keyed by `escrow_id` so that subsequent
    /// `/escrow/download/...` requests can hand them out.
    pub async fn escrow_split(
        &self,
        file_bytes: Vec<u8>,
        filename: String,
        k: usize,
        n: usize,
    ) -> Result<EscrowSplitResult, GatewayError> {
        if file_bytes.is_empty() {
            return Err(GatewayError::BadRequest("empty file".into()));
        }
        if !(1..=64).contains(&k) || !(k..=64).contains(&n) {
            return Err(GatewayError::BadRequest(
                "invalid K/N (1 ≤ K ≤ N ≤ 64)".into(),
            ));
        }
        let content_type = guess_opaque_content_type(&filename);
        let source_bytes = file_bytes.len();
        let params = holofs_analytics::escrow::EscrowParams {
            k,
            n,
            content_type,
            filename: filename.clone(),
        };
        let shares = holofs_analytics::escrow::split_into_shares(&file_bytes, &params);
        let escrow_id_hex = hex(&shares[0].escrow_id);
        let infos: Vec<EscrowShareInfo> = shares
            .iter()
            .enumerate()
            .map(|(i, sh)| EscrowShareInfo {
                idx: i as u16,
                download_path: format!("{escrow_id_hex}_{i}.holoshare"),
                filename: format!("share_{i:02}_of_{n}.holoshare"),
                bytes: sh.encode().len(),
            })
            .collect();
        self.escrow_cache
            .lock()
            .await
            .insert(escrow_id_hex.clone(), shares);
        Ok(EscrowSplitResult {
            filename,
            source_bytes,
            k,
            n,
            escrow_id_hex,
            shares: infos,
        })
    }

    /// Fetch one share by `escrow_id` hex + index. Returns `NotFound` when
    /// the gateway has been restarted (shares only live in RAM).
    pub async fn escrow_download(
        &self,
        escrow_id_hex: &str,
        idx: usize,
    ) -> Result<EscrowShareBytes, GatewayError> {
        let cache = self.escrow_cache.lock().await;
        let shares = cache.get(escrow_id_hex).ok_or_else(|| {
            GatewayError::NotFound
        })?;
        let share = shares
            .get(idx)
            .ok_or_else(|| GatewayError::BadRequest(format!("no share index {idx}")))?;
        Ok(EscrowShareBytes {
            idx: idx as u16,
            total_n: share.total_n,
            bytes: share.encode(),
        })
    }

    /// Recover the original file from a set of `.holoshare` blobs. The
    /// gateway does not need the cache for this — recovery is stateless and
    /// works as long as `k` valid shares from the same escrow are supplied.
    pub async fn escrow_recover(
        &self,
        share_blobs: Vec<Vec<u8>>,
    ) -> Result<EscrowRecoverResult, GatewayError> {
        let mut shares: Vec<holofs_analytics::escrow::ShareFile> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for (i, bytes) in share_blobs.into_iter().enumerate() {
            if bytes.is_empty() {
                continue;
            }
            match holofs_analytics::escrow::ShareFile::decode(&bytes) {
                Ok(sh) => shares.push(sh),
                Err(e) => errors.push(format!("share #{i}: {e}")),
            }
        }
        if shares.is_empty() {
            return Err(GatewayError::BadRequest(format!(
                "no valid .holoshare files ({})",
                errors.join("; ")
            )));
        }
        let shares_used = shares.len();
        let (data, content_type, filename) = holofs_analytics::escrow::recover_from_shares(&shares)
            .map_err(|e| GatewayError::BadRequest(format!("recover failed: {e}")))?;
        Ok(EscrowRecoverResult {
            data,
            content_type,
            filename,
            shares_used,
        })
    }
}

