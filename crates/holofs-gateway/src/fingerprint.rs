//! Perceptual fingerprints + similarity ranking.
//!
//! - [`Gateway::fingerprint_of`] — 16-byte perceptual hash of one
//!   object (`GET /api/fingerprint/<name>`). Image/audio: L1 dHash
//!   over L0 systematic shards. Text/opaque: first bytes of the CID.
//! - [`Gateway::compute_fingerprint_for`] — same computation, but
//!   takes an already-loaded [`Manifest`] (used by `holofs-web`
//!   listing pages that want to render fingerprints for every row
//!   without re-fetching manifests).
//! - [`Gateway::similar_to`] — `/similar/<name>` view-model: top-10
//!   perceptual neighbours of the same kind + cross-object shard
//!   overlaps. `scope` restricts the candidate pool to the target's
//!   parent directory / subtree / whole catalog.
//!
//! `compute_fingerprint` (private) is the shared workhorse both
//! methods call — one place to change how L0 shards get gathered
//! and verified.
//!
//! Moved out of `http_gateway.rs` in Phase R1b.15.

use holofs_model::manifest::{Manifest, ObjectKind};

use crate::error::GatewayError;
use crate::similarity::{
    in_scope, parent_dir, ShardOverlap, SimilarMatch, SimilarReport, SimilarScope,
    SimilarityMethod,
};
use crate::Gateway;

/// Result of [`Gateway::fingerprint_of`].
#[derive(Debug, Clone)]
pub struct FingerprintInfo {
    /// Catalog name.
    pub name: String,
    /// 32-char lowercase hex of the 16-byte fingerprint.
    pub fingerprint_hex: String,
    /// Object kind — controls how the fingerprint was computed.
    pub kind: ObjectKind,
}

impl Gateway {
    /// Compute the object's perceptual fingerprint. Fetches shards (channel=0,
    /// layer=0) from live nodes, filters by hash, then calls
    /// [`holofs_analytics::fingerprint::perceptual_fingerprint`]. For opaque/text
    /// the fingerprint degenerates to the first bytes of the CID.
    pub(crate) async fn compute_fingerprint(
        &self,
        manifest: &Manifest,
    ) -> holofs_analytics::fingerprint::Fingerprint {
        if matches!(manifest.kind, ObjectKind::Opaque | ObjectKind::Text) {
            return holofs_analytics::fingerprint::perceptual_fingerprint(manifest, &[]);
        }
        let live = self.effective_live().await;
        // Stage 11.6: gather L0 for every channel (up to 3 — fingerprint
        // covers RGB). Per-channel means power the new 45-bit dHash; a
        // single-channel fingerprint clustered too many unrelated images
        // at 95%+ similarity. The per-(name, channel, layer) cache (Stage
        // 11.2) deduplicates concurrent gathers for the same object.
        let n_channels = (manifest.channels as usize).min(3);
        let mut per_channel: Vec<Vec<holofs_core::rlnc::Shard>> =
            Vec::with_capacity(n_channels);
        for c in 0..n_channels {
            let shards = holofs_client::gather_layer(manifest, &live, c as u8, 0)
                .await
                .unwrap_or_default();
            let expected: std::collections::HashSet<_> = manifest
                .shard_hashes
                .get(c)
                .and_then(|cl| cl.first())
                .map(|hs| hs.iter().copied().collect())
                .unwrap_or_default();
            let verified: Vec<holofs_core::rlnc::Shard> = shards
                .into_iter()
                .filter(|s| expected.contains(&holofs_core::merkle::shard_hash(s)))
                .collect();
            per_channel.push(verified);
        }
        holofs_analytics::fingerprint::perceptual_fingerprint(manifest, &per_channel)
    }

    /// Perceptual fingerprint of an object for `GET /api/fingerprint/<name>`.
    /// Image/audio: 16-byte L1 hash from L0 systematic shards. Text/opaque:
    /// fallback to the first 16 bytes of `data_cid`.
    pub async fn fingerprint_of(&self, name: &str) -> Result<FingerprintInfo, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let fp = self.compute_fingerprint(&manifest).await;
        Ok(FingerprintInfo {
            name: name.to_string(),
            fingerprint_hex: holofs_analytics::fingerprint::fingerprint_hex(&fp),
            kind: manifest.kind,
        })
    }

    /// Compute the object's perceptual fingerprint. Exposed for the
    /// `/similar/<name>` view-model in `holofs-web`. Image/audio reach into the
    /// L0 systematic shards; text/opaque fall back to the first 16 bytes of
    /// `data_cid`.
    pub async fn compute_fingerprint_for(
        &self,
        manifest: &Manifest,
    ) -> holofs_analytics::fingerprint::Fingerprint {
        self.compute_fingerprint(manifest).await
    }

    /// `/similar/<name>` view-model: top-10 neighbours of the same kind +
    /// cross-object shard overlaps (any kind). `scope` constrains the
    /// candidate pool relative to the target's parent directory — `All`
    /// scans the whole catalog (current behavior), `Folder` keeps only
    /// direct siblings, `Tree` keeps the subtree rooted at the parent.
    pub async fn similar_to(
        &self,
        name: &str,
        scope: SimilarScope,
    ) -> Result<SimilarReport, GatewayError> {
        let snapshot = self.catalog.lock().await.clone();
        let manifest = snapshot
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let is_text = manifest.kind == ObjectKind::Text;
        let target_fp = if is_text {
            [0u8; holofs_analytics::fingerprint::FP_LEN]
        } else {
            self.compute_fingerprint(&manifest).await
        };
        let target_parent = parent_dir(name);

        let mut neighbors: Vec<SimilarMatch> = Vec::new();
        for n in snapshot.names() {
            if n == name {
                continue;
            }
            if !in_scope(target_parent, &n, scope) {
                continue;
            }
            let m = match snapshot.get(&n) {
                Some(m) => m,
                None => continue,
            };
            if m.kind != manifest.kind {
                continue;
            }
            let (sim, method) = if is_text {
                let j = holofs_analytics::shingle::jaccard_similarity(
                    &manifest.text_minhash,
                    &m.text_minhash,
                );
                (j * 100.0, SimilarityMethod::Jaccard)
            } else {
                let fp = self.compute_fingerprint(m).await;
                let sim =
                    holofs_analytics::fingerprint::fingerprint_similarity_pct(&target_fp, &fp);
                (sim, SimilarityMethod::DHash)
            };
            neighbors.push(SimilarMatch {
                name: n,
                similarity_pct: sim,
                method,
            });
        }
        neighbors.sort_by(|a, b| {
            b.similarity_pct
                .partial_cmp(&a.similarity_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        neighbors.truncate(10);

        let mut overlaps: Vec<ShardOverlap> = Vec::new();
        let total_a = manifest.shard_hashes.iter().flatten().flatten().count();
        for n in snapshot.names() {
            if n == name {
                continue;
            }
            if !in_scope(target_parent, &n, scope) {
                continue;
            }
            let other = match snapshot.get(&n) {
                Some(o) => o,
                None => continue,
            };
            let (common, _ta, _tb) =
                holofs_analytics::fingerprint::shard_overlap(&manifest, other);
            if common > 0 {
                let pct = if total_a > 0 {
                    common as f32 * 100.0 / total_a as f32
                } else {
                    0.0
                };
                // Stage 13.0: compute low / high band overlaps so the
                // UI can surface a "robust copy?" warning. Split point
                // is the midpoint of `nlayers` — for `nlayers=8` that's
                // L0..=L3 vs L4..=L7. For files with `nlayers < 2`
                // (text / opaque) both bands collapse to zero, which
                // the UI treats as "n/a".
                let per_layer =
                    holofs_analytics::fingerprint::shard_overlap_per_layer(&manifest, other);
                let nlayers = per_layer.len();
                let mid = nlayers / 2;
                let low_shared: u32 = per_layer.iter().take(mid).sum();
                let high_shared: u32 = per_layer.iter().skip(mid).sum();
                let low_total: usize = manifest
                    .shard_hashes
                    .iter()
                    .flat_map(|chan| chan.iter().take(mid))
                    .map(|hs| hs.len())
                    .sum();
                let high_total: usize = manifest
                    .shard_hashes
                    .iter()
                    .flat_map(|chan| chan.iter().skip(mid))
                    .map(|hs| hs.len())
                    .sum();
                let low_pct = if low_total > 0 {
                    low_shared as f32 * 100.0 / low_total as f32
                } else {
                    0.0
                };
                let high_pct = if high_total > 0 {
                    high_shared as f32 * 100.0 / high_total as f32
                } else {
                    0.0
                };
                overlaps.push(ShardOverlap {
                    name: n,
                    common,
                    overlap_pct: pct,
                    low_layer_overlap_pct: low_pct,
                    high_layer_overlap_pct: high_pct,
                    robust_copy_score: low_pct - high_pct,
                });
            }
        }
        overlaps.sort_by_key(|o| std::cmp::Reverse(o.common));

        let fingerprint_hex = if is_text {
            holofs_analytics::shingle::minhash_hex_preview(&manifest.text_minhash)
        } else {
            holofs_analytics::fingerprint::fingerprint_hex(&target_fp)
        };

        Ok(SimilarReport {
            name: name.to_string(),
            kind: manifest.kind,
            fingerprint_hex,
            minhash_k: holofs_analytics::shingle::MINHASH_K,
            total_shards: total_a,
            neighbors,
            overlaps,
        })
    }
}
