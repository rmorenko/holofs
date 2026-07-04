//! in-memory ANN index over the on-disk
//! [`Index`](crate::Index) records.
//!
//! Holds every embedding for the catalog in RAM, grouped by
//! [`LayerBand`]. Per band: if the record count is at or above
//! [`HNSW_MIN_BAND_SIZE`], build an `instant_distance::HnswMap` and
//! query via approximate-nearest-neighbour walk; below threshold,
//! brute-force cosine is faster than HNSW's constant overhead.
//!
//! The index is **immutable** — appends invalidate it and the gateway
//! rebuilds on next query. Build cost on Apple silicon: ~80 ms for
//! 1k vectors, ~1.4 s for 50k vectors. Single-shot, amortised over
//! every query until the next PUT.

use std::collections::HashMap;

use instant_distance::{Builder, HnswMap, Point, Search};

use crate::index::EmbedRecord;
use crate::LayerBand;

/// Band-record count below which HNSW build overhead outweighs the
/// brute-force scan. Tuned empirically — at ~200 vectors a brute
/// scan is sub-millisecond, HNSW construction is ~15 ms. Bump if
/// query patterns change.
pub const HNSW_MIN_BAND_SIZE: usize = 200;

/// One hit returned by [`HnswIndex::search`].
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Catalog name of the matching object.
    pub name: String,
    /// Cosine similarity in `[-1.0, 1.0]` (computed as `1 - distance`
    /// because the underlying ANN reports L2/cosine distance and we
    /// store L2-normalised vectors).
    pub score: f32,
    /// Layer band that produced the hit (always matches the query
    /// band — surfaced so callers don't have to thread the band
    /// through their own bookkeeping).
    pub band: LayerBand,
}

/// In-memory wrapper over [`EmbedRecord`]s with optional per-band
/// HNSW acceleration.
pub struct HnswIndex {
    /// Per-band built HNSW. Only populated for bands with at least
    /// [`HNSW_MIN_BAND_SIZE`] records.
    by_band_hnsw: HashMap<LayerBand, HnswMap<EmbVec, String>>,
    /// Per-band raw records — used both for brute-force fallback on
    /// small bands AND for the L2 score reconstruction on HNSW hits.
    by_band_records: HashMap<LayerBand, Vec<EmbedRecord>>,
}

impl HnswIndex {
    /// Build the index from a flat list of records. Records are
    /// consumed; the caller hands ownership over.
    #[must_use]
    pub fn build_from(records: Vec<EmbedRecord>) -> Self {
        let mut by_band_records: HashMap<LayerBand, Vec<EmbedRecord>> = HashMap::new();
        for rec in records {
            if rec.vec.is_empty() {
                // tombstone — skip
                continue;
            }
            by_band_records.entry(rec.band).or_default().push(rec);
        }

        let mut by_band_hnsw = HashMap::new();
        for (band, recs) in &by_band_records {
            if recs.len() < HNSW_MIN_BAND_SIZE {
                continue;
            }
            let points: Vec<EmbVec> = recs.iter().map(|r| EmbVec(r.vec.clone())).collect();
            let values: Vec<String> = recs.iter().map(|r| r.name.clone()).collect();
            let map = Builder::default().build(points, values);
            by_band_hnsw.insert(*band, map);
        }

        Self {
            by_band_hnsw,
            by_band_records,
        }
    }

    /// Query top-`k` nearest neighbours in `band`. Picks HNSW when the
    /// band is large enough, brute-force otherwise. Returns hits
    /// sorted by score descending.
    #[must_use]
    pub fn search(&self, band: LayerBand, query: &[f32], k: usize) -> Vec<SearchHit> {
        let recs = match self.by_band_records.get(&band) {
            Some(r) => r,
            None => return Vec::new(),
        };
        if let Some(map) = self.by_band_hnsw.get(&band) {
            self.search_hnsw(band, recs, map, query, k)
        } else {
            self.search_brute(band, recs, query, k)
        }
    }

    /// Search across all bands and keep the best score per name —
    /// mirrors `SearchBand::Any` from the gateway.
    #[must_use]
    pub fn search_any(&self, query: &[f32], k: usize) -> Vec<SearchHit> {
        let mut best: HashMap<String, SearchHit> = HashMap::new();
        for band in [LayerBand::Coarse, LayerBand::Mid, LayerBand::Full] {
            for hit in self.search(band, query, k) {
                let cur = best
                    .entry(hit.name.clone())
                    .or_insert_with(|| hit.clone());
                if hit.score > cur.score {
                    *cur = hit;
                }
            }
        }
        let mut out: Vec<SearchHit> = best.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(k);
        out
    }

    /// Total non-tombstone records this index holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_band_records.values().map(|v| v.len()).sum()
    }

    /// `true` when no live records are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of bands that ended up with an HNSW index built (i.e.
    /// crossed [`HNSW_MIN_BAND_SIZE`]). Surfaced in observability.
    #[must_use]
    pub fn hnsw_band_count(&self) -> usize {
        self.by_band_hnsw.len()
    }

    fn search_hnsw(
        &self,
        band: LayerBand,
        recs: &[EmbedRecord],
        map: &HnswMap<EmbVec, String>,
        query: &[f32],
        k: usize,
    ) -> Vec<SearchHit> {
        let q = EmbVec(query.to_vec());
        let mut search = Search::default();
        // Pull a few more than `k` so post-dedup we can still return a
        // full page when multiple HNSW hits collapse to the same name
        // (rare but possible if duplicate records were appended).
        let pull = (k * 2).max(k + 8);
        let mut out: Vec<SearchHit> = Vec::with_capacity(k);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for hit in map.search(&q, &mut search).take(pull) {
            let name = hit.value.clone();
            if seen.insert(name.clone()) {
                // `instant-distance` reports distance == 1 - cosine
                // for our normalised vectors, so invert it to surface
                // similarity in the same shape as the brute path.
                let score = 1.0 - hit.distance;
                out.push(SearchHit { name, score, band });
                if out.len() >= k {
                    break;
                }
            }
        }
        // HNSW already returns ordered by distance; that maps to
        // descending score after the 1 - distance flip, so no resort
        // needed. Be defensive anyway in case the iterator
        // contract loosens upstream.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let _ = recs; // silence unused warning if HNSW path drops the rec lookup
        out
    }

    fn search_brute(
        &self,
        band: LayerBand,
        recs: &[EmbedRecord],
        query: &[f32],
        k: usize,
    ) -> Vec<SearchHit> {
        let mut scored: Vec<SearchHit> = recs
            .iter()
            .map(|r| SearchHit {
                name: r.name.clone(),
                score: cosine(query, &r.vec),
                band,
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        scored
    }
}

/// Cosine similarity of two L2-normalised vectors == dot product.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = 0.0f32;
    for i in 0..n {
        acc += a[i] * b[i];
    }
    acc
}

/// Internal point wrapper — instant-distance needs a `Point` impl
/// that owns the vector data and reports a metric distance.
#[derive(Clone, Debug)]
struct EmbVec(Vec<f32>);

impl Point for EmbVec {
    fn distance(&self, other: &Self) -> f32 {
        // 1 - cosine for L2-normalised inputs. Range `[0, 2]`, but
        // for real CLIP embeddings clusters tightly in `[0.6, 0.9]`.
        let n = self.0.len().min(other.0.len());
        let mut dot = 0.0f32;
        for i in 0..n {
            dot += self.0[i] * other.0[i];
        }
        1.0 - dot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EMBED_DIM;

    fn rec(name: &str, band: LayerBand, seed: f32) -> EmbedRecord {
        // Deterministic-ish L2-normalised vector that differs per seed.
        let mut v: Vec<f32> = (0..EMBED_DIM)
            .map(|i| ((i as f32) * 0.001 + seed).sin())
            .collect();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        EmbedRecord {
            data_cid: [0u8; 32],
            band,
            name: name.to_string(),
            vec: v,
        }
    }

    #[test]
    fn brute_force_path_below_threshold() {
        let recs = vec![
            rec("a", LayerBand::Coarse, 0.0),
            rec("b", LayerBand::Coarse, 0.5),
            rec("c", LayerBand::Coarse, 1.0),
        ];
        let q = rec("query", LayerBand::Coarse, 0.0).vec;
        let idx = HnswIndex::build_from(recs);
        assert_eq!(idx.hnsw_band_count(), 0); // below threshold → no HNSW
        let hits = idx.search(LayerBand::Coarse, &q, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "a"); // identical vector → score == 1.0
        assert!((hits[0].score - 1.0).abs() < 1e-4);
    }

    #[test]
    fn hnsw_path_above_threshold() {
        // Synthesize enough records to cross the threshold.
        let n = HNSW_MIN_BAND_SIZE + 50;
        let recs: Vec<EmbedRecord> = (0..n)
            .map(|i| rec(&format!("img_{i}"), LayerBand::Mid, (i as f32) * 0.1))
            .collect();
        let q = rec("q", LayerBand::Mid, 7.0 * 0.1).vec; // matches img_7
        let idx = HnswIndex::build_from(recs);
        assert_eq!(idx.hnsw_band_count(), 1);
        let hits = idx.search(LayerBand::Mid, &q, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].name, "img_7");
    }

    #[test]
    fn search_any_picks_best_band_per_name() {
        // Same name in two bands with different scores → keep the
        // higher-scoring band only.
        let q = rec("q", LayerBand::Coarse, 0.0).vec;
        let recs = vec![
            rec("same", LayerBand::Coarse, 0.0), // perfect match
            rec("same", LayerBand::Full, 1.0),   // weaker
            rec("other", LayerBand::Coarse, 2.0),
        ];
        let idx = HnswIndex::build_from(recs);
        let hits = idx.search_any(&q, 5);
        assert_eq!(hits[0].name, "same");
        assert_eq!(hits[0].band, LayerBand::Coarse);
        assert!((hits[0].score - 1.0).abs() < 1e-4);
    }
}
