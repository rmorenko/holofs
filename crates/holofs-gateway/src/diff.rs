//! `/diff/<a>/<b>` chunk-diff analyzer — a byte-perfect dedup view.
//!
//! Two systematic chunks are marked common only when their shard
//! hashes match — i.e. the underlying source bytes are identical.
//! -11.8 experimented with perceptual variants (mean-based,
//! then per-coefficient L1) to make blurred copies "look closer",
//! but every threshold had pathological neighbours: desaturation
//! that preserves luminance scored higher than blur; mandala
//! outscored real photos. Perceptual ranking is what
//! [`Gateway::similar_to`] is for; `/diff` stays the dedup tool.
//!

use crate::error::GatewayError;
use crate::Gateway;

/// One coloured cell of the `/diff/<a>/<b>` grid.
#[derive(Debug, Clone, Copy)]
pub struct DiffCell {
    /// Shard index inside the (channel, layer) bucket.
    pub idx: u32,
    /// `true` when the chunk-hash matches between both objects.
    pub is_common: bool,
}

/// One row of the per-layer chunk-diff grid.
#[derive(Debug, Clone)]
pub struct DiffLayer {
    /// Channel index.
    pub channel: u8,
    /// Layer index.
    pub layer: u8,
    /// Cells marked `is_common`.
    pub n_common: usize,
    /// Total cells in the row.
    pub n_total: usize,
    /// One cell per systematic chunk.
    pub cells: Vec<DiffCell>,
}

/// Result of [`Gateway::diff_chunks`].
#[derive(Debug, Clone)]
pub struct DiffReport {
    /// Catalog name of object A.
    pub name_a: String,
    /// Catalog name of object B.
    pub name_b: String,
    /// Common object kind (diff is rejected when A and B kinds differ).
    pub kind: holofs_model::manifest::ObjectKind,
    /// Total common chunks across all layers.
    pub common: usize,
    /// Total compared chunks (across all `min(channels, layers, k)` cells).
    pub total: usize,
    /// `common / total * 100`.
    pub similarity_pct: f32,
    /// `common * sym_len[0]` — approximate bytes saved by dedup.
    pub storage_saved_bytes: u64,
    /// One entry per (channel, layer), in `BTreeMap` order.
    pub layers: Vec<DiffLayer>,
}

impl Gateway {
    /// `/diff/<a>/<b>` view-model: per-(channel, layer) chunk diff cells +
    /// aggregated counters.
    pub async fn diff_chunks(
        &self,
        name_a: &str,
        name_b: &str,
    ) -> Result<DiffReport, GatewayError> {
        let snapshot = self.catalog.read().await.clone();
        let a = snapshot
            .get(name_a)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        let b = snapshot
            .get(name_b)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if a.kind != b.kind {
            return Err(GatewayError::BadRequest(
                "diff: both objects must be of the same kind".into(),
            ));
        }

        let diff = holofs_analytics::fingerprint::chunk_diff(&a, &b);

        // Group cells by (channel, layer).
        let mut by_layer: std::collections::BTreeMap<(u8, u8), Vec<DiffCell>> =
            std::collections::BTreeMap::new();
        for e in &diff.entries {
            by_layer
                .entry((e.channel, e.layer))
                .or_default()
                .push(DiffCell {
                    idx: e.idx,
                    is_common: e.is_common,
                });
        }
        let layers: Vec<DiffLayer> = by_layer
            .into_iter()
            .map(|((c, l), cells)| {
                let n_common = cells.iter().filter(|x| x.is_common).count();
                let n_total = cells.len();
                DiffLayer {
                    channel: c,
                    layer: l,
                    n_common,
                    n_total,
                    cells,
                }
            })
            .collect();

        let sym_len_l0 = a.sym_len.first().copied().unwrap_or(0) as u64;
        let storage_saved_bytes = diff.common as u64 * sym_len_l0;

        Ok(DiffReport {
            name_a: name_a.to_string(),
            name_b: name_b.to_string(),
            kind: a.kind,
            common: diff.common,
            total: diff.total,
            similarity_pct: diff.similarity_pct(),
            storage_saved_bytes,
            layers,
        })
    }
}
