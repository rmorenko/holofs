//! Stage 12.7 — per-file business-meaningful metrics.
//!
//! [`Gateway::file_metrics`] collapses three signal classes into one
//! server roundtrip:
//!
//! - **Storage / dedup** — total vs unique shard hashes inside the
//!   file and inside the catalog.
//! - **Originality + structural neighbours** — for every other file
//!   we tally `(shared_total, shared_per_layer)` against the target
//!   so the UI can distinguish "same composition, different detail"
//!   from "same texture, different composition".
//! - **Layer energy** — image / audio only. Decodes every layer once
//!   and sums squared coefficient magnitudes so the UI can render a
//!   detail score and (for audio) a bass / mid / treble split.
//!
//! Moved out of `http_gateway.rs` in Phase R1b.14.

use holofs_client::layer_energies;
use holofs_model::manifest::ObjectKind;

use crate::error::GatewayError;
use crate::Gateway;

/// One row of [`FileMetrics::neighbours`] — a catalog entry that shares
/// at least one shard hash with the target file. Holofs's content-addressed
/// shard storage means we can describe "how much of file A also lives in
/// file B" with no decoding and no perceptual hashing.
#[derive(Debug, Clone)]
pub struct NeighbourMetric {
    /// Catalog name of the other file.
    pub name: String,
    /// Object kind, so the UI can render different glyphs for image/audio/etc.
    pub kind: ObjectKind,
    /// Total shard hashes shared (summed across channels and layers).
    pub shared_total: u64,
    /// Per-layer split of `shared_total`, indexed by *this* file's layer
    /// (`shared_per_layer.len() == target.nlayers`). Lets the UI tell apart
    /// "shares the coarse structure" from "shares the fine details".
    pub shared_per_layer: Vec<u32>,
    /// `shared_total` as a percentage of the target's unique hashes.
    pub overlap_pct: f32,
}

/// Result of [`Gateway::file_metrics`] — bundle of business-meaningful
/// signals about one catalog entry that fall out of the holofs storage
/// geometry (per-layer shards + content-addressed hashes) for free.
#[derive(Debug, Clone)]
pub struct FileMetrics {
    /// Catalog name of the target file.
    pub name: String,
    /// Object kind.
    pub kind: ObjectKind,

    // --- Storage / dedup -------------------------------------------------
    /// Total shard hashes the file would occupy if every hash were unique.
    pub total_shards_in_file: u64,
    /// Distinct shard hashes inside the file (≤ total; equal in the common
    /// case — drops below total only when an RLNC encoding happens to
    /// repeat a hash, which is rare).
    pub unique_shards_in_file: u64,
    /// `(total - unique) / total * 100`.
    pub file_dedup_savings_pct: f32,
    /// Catalog-wide total shards.
    pub catalog_total_shards: u64,
    /// Distinct shard hashes across the entire catalog (cluster-wide
    /// dedup denominator).
    pub catalog_unique_shards: u64,

    // --- Originality -----------------------------------------------------
    /// Number of this file's distinct hashes that do not appear in any
    /// other catalog entry — the file's exclusive contribution.
    pub unique_to_file: u64,
    /// `unique_to_file / distinct_hashes_in_file * 100`.
    pub originality_pct: f32,
    /// Per-layer originality percentage (length = `nlayers`). High values
    /// in low layers ⇒ unique structure; high values in high layers ⇒
    /// unique detail. The combination is the storytelling axis.
    pub originality_per_layer: Vec<f32>,

    // --- Cross-file reuse ------------------------------------------------
    /// Top-N (currently 8) other catalog entries with the most shared
    /// hashes, sorted by `shared_total` descending. Empty if the file
    /// has no neighbours.
    pub neighbours: Vec<NeighbourMetric>,

    // --- Decoded layer energy (image / audio only) ----------------------
    /// Sum of squared DWT coefficients per layer (channels summed).
    /// `None` when not applicable (Text, Opaque) or when the decode
    /// failed (e.g. too few live shards).
    pub layer_energy: Option<Vec<f64>>,
    /// Audio-only: three-band energy split derived from `layer_energy`.
    pub audio_bands: Option<AudioBandEnergy>,
}

/// Three-band audio energy split returned in [`FileMetrics::audio_bands`].
/// Bands are an equal-thirds partition of the layer set, mapped to
/// rough frequency labels: layer 0 = bass envelope, the top layers =
/// treble transients.
#[derive(Debug, Clone, Copy)]
pub struct AudioBandEnergy {
    /// Energy in the bottom third of the layer set (bass / envelope).
    pub bass: f64,
    /// Energy in the middle third of the layer set.
    pub mid: f64,
    /// Energy in the top third of the layer set (treble / transients).
    pub treble: f64,
}

impl Gateway {
    /// Stage 12.7: per-file metrics for `/health/<name>`.
    ///
    /// Three classes of signal collapsed into one server roundtrip:
    ///
    /// 1. **Storage / deduplication** — count of unique shard hashes
    ///    inside this file, inside the catalog as a whole, and how many
    ///    of *this* file's hashes are not seen anywhere else. Gives a
    ///    direct «how much exclusive content this file contributes»
    ///    number that is impossible to get from raw byte counts.
    ///
    /// 2. **Originality + structural neighbours** — for every other
    ///    file we tally `(shared_total, shared_per_layer)` against the
    ///    target. Two files that share the coarsest layers but diverge
    ///    higher up are "same composition, different detail"; the
    ///    inverse pattern says "same texture / detail, different
    ///    composition". The frontend renders the per-layer split as a
    ///    little bar per neighbour so the kind of similarity is
    ///    legible at a glance.
    ///
    /// 3. **Layer energy** — for image / audio only. We actually decode
    ///    every layer once and sum squared coefficient magnitudes; the
    ///    distribution lets the UI report a "detail score" (energy
    ///    above the median layer / total) and, for audio, a
    ///    bass / mid / treble breakdown via even thirds of the layer
    ///    set.
    pub async fn file_metrics(&self, name: &str) -> Result<FileMetrics, GatewayError> {
        let snapshot = self.catalog.lock().await.clone();
        let manifest = snapshot
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind == ObjectKind::Directory {
            return Err(GatewayError::BadRequest(
                "file_metrics: not applicable to directories".into(),
            ));
        }
        let nlayers = manifest.nlayers as usize;

        // --- 1. Flatten *this file*'s shard hashes per layer ---------------
        // `layer_hashes[l] = set of hashes across all channels of layer l`.
        // We dedupe within the file too so a duplicated coefficient (rare,
        // but possible when sym_len is short) doesn't get double-counted.
        let mut layer_hashes: Vec<std::collections::HashSet<[u8; 32]>> =
            vec![std::collections::HashSet::new(); nlayers];
        let mut total_in_file: u64 = 0;
        for c in 0..manifest.channels as usize {
            for l in 0..nlayers {
                if let Some(hs) = manifest
                    .shard_hashes
                    .get(c)
                    .and_then(|chan| chan.get(l))
                {
                    total_in_file += hs.len() as u64;
                    for h in hs {
                        layer_hashes[l].insert(*h);
                    }
                }
            }
        }
        let unique_in_file: u64 = layer_hashes.iter().map(|s| s.len() as u64).sum();
        let all_in_file: std::collections::HashSet<[u8; 32]> =
            layer_hashes.iter().flatten().copied().collect();

        // --- 2. Walk other catalog manifests once -------------------------
        //
        // For each other file we accumulate, per layer of *this* file,
        // how many of its hashes also show up in that other file. The
        // sum across layers is `shared_total`. We also track which of
        // this file's hashes appear in any other file at all, which
        // gives us the per-layer + overall originality counts.
        let mut foreign_seen: std::collections::HashSet<[u8; 32]> =
            std::collections::HashSet::new();
        let mut catalog_total_shards: u64 = 0;
        let mut catalog_unique_set: std::collections::HashSet<[u8; 32]> =
            std::collections::HashSet::new();
        let mut neighbours: Vec<NeighbourMetric> = Vec::new();

        for n in snapshot.names() {
            let other = match snapshot.get(&n) {
                Some(m) => m,
                None => continue,
            };
            if other.kind == ObjectKind::Directory {
                continue;
            }
            // Catalog-wide accounting (always — includes self).
            for c in 0..other.channels as usize {
                let nlayers_o = other.nlayers as usize;
                for l in 0..nlayers_o {
                    if let Some(hs) = other
                        .shard_hashes
                        .get(c)
                        .and_then(|chan| chan.get(l))
                    {
                        catalog_total_shards += hs.len() as u64;
                        for h in hs {
                            catalog_unique_set.insert(*h);
                        }
                    }
                }
            }
            if n == name {
                continue;
            }
            // Neighbour: per-(this-file) layer shared count.
            let mut per_layer = vec![0u32; nlayers];
            let mut shared_total: u64 = 0;
            for c in 0..other.channels as usize {
                let nlayers_o = other.nlayers as usize;
                for l in 0..nlayers_o {
                    if let Some(hs) = other
                        .shard_hashes
                        .get(c)
                        .and_then(|chan| chan.get(l))
                    {
                        for h in hs {
                            if all_in_file.contains(h) {
                                foreign_seen.insert(*h);
                                shared_total += 1;
                                // Bucket the shared hash under the layer of
                                // *this* file it lives in.
                                for (li, set) in layer_hashes.iter().enumerate().take(nlayers) {
                                    if set.contains(h) {
                                        per_layer[li] += 1;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if shared_total > 0 {
                let pct = if unique_in_file > 0 {
                    shared_total as f32 * 100.0 / unique_in_file as f32
                } else {
                    0.0
                };
                neighbours.push(NeighbourMetric {
                    name: n,
                    kind: other.kind,
                    shared_total,
                    shared_per_layer: per_layer,
                    overlap_pct: pct,
                });
            }
        }
        neighbours.sort_by_key(|m| std::cmp::Reverse(m.shared_total));
        neighbours.truncate(8);

        let unique_to_file: u64 = all_in_file
            .iter()
            .filter(|h| !foreign_seen.contains(*h))
            .count() as u64;
        let originality_pct = if !all_in_file.is_empty() {
            unique_to_file as f32 * 100.0 / all_in_file.len() as f32
        } else {
            0.0
        };
        // Per-layer originality: of this layer's hashes, how many are NOT in any
        // other file?
        let mut originality_per_layer: Vec<f32> = Vec::with_capacity(nlayers);
        for set in &layer_hashes {
            if set.is_empty() {
                originality_per_layer.push(0.0);
            } else {
                let exclusive = set.iter().filter(|h| !foreign_seen.contains(*h)).count();
                originality_per_layer
                    .push(exclusive as f32 * 100.0 / set.len() as f32);
            }
        }

        // --- 3. Layer energy (image / audio only) -------------------------
        //
        // The decode does a network roundtrip per (channel, layer) — same
        // weight as a Monte Carlo health pass — so we gate it on object
        // kind. Anything that can't be Haar-decoded (Opaque, Text)
        // returns `None` and the UI hides the block.
        let layer_energy = if matches!(manifest.kind, ObjectKind::Image | ObjectKind::Audio) {
            let live = self.effective_live().await;
            match layer_energies(&self.gf, &manifest, &live).await {
                Ok((energies, _bytes)) => Some(energies),
                Err(_) => None,
            }
        } else {
            None
        };

        // For audio, group layers into three equal bands. Layer 0 = coarsest
        // = bass envelope; the highest layer = treble. We sum energies
        // across consecutive layer bins.
        let audio_bands = if manifest.kind == ObjectKind::Audio {
            if let Some(energies) = layer_energy.as_ref() {
                let n = energies.len();
                if n == 0 {
                    None
                } else {
                    let third = (n + 2) / 3;
                    let bass = energies.iter().take(third).sum::<f64>();
                    let mid = energies
                        .iter()
                        .skip(third)
                        .take(third)
                        .sum::<f64>();
                    let treble = energies.iter().skip(2 * third).sum::<f64>();
                    Some(AudioBandEnergy { bass, mid, treble })
                }
            } else {
                None
            }
        } else {
            None
        };

        Ok(FileMetrics {
            name: name.to_string(),
            kind: manifest.kind,
            // Storage / dedup.
            total_shards_in_file: total_in_file,
            unique_shards_in_file: unique_in_file,
            file_dedup_savings_pct: if total_in_file > 0 {
                (total_in_file - unique_in_file) as f32 * 100.0 / total_in_file as f32
            } else {
                0.0
            },
            catalog_total_shards,
            catalog_unique_shards: catalog_unique_set.len() as u64,
            unique_to_file,
            originality_pct,
            originality_per_layer,
            // Cross-file reuse.
            neighbours,
            // Decoded.
            layer_energy,
            audio_bands,
        })
    }
}
