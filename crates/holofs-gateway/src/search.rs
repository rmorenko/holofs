//! CLIP-based semantic search.
//!
//! Owns the on-line embedding pipeline: `ensure_embedder` loads
//! CLIP-multilingual on first use, `embed_object` decodes an image
//! and appends per-band embeddings to `embeddings.bin`,
//! `semantic_search` builds (or reuses) an in-memory
//! [`HnswIndex`](holofs_embed::HnswIndex) and returns top-K hits.
//!
//! The lazy `EmbedState` also holds the ANN generation counter used
//! by the GC path to invalidate the cached index whenever a PUT
//! bumps `embeddings.bin`. Moved out of `http_gateway.rs` in Phase
//! R1b.2 so the search surface no longer clutters the monolith.

use std::sync::Arc;

use holofs_model::manifest::ObjectKind;

use crate::error::GatewayError;
use crate::Gateway;

/// Lazy embedder state shared across the gateway. `enabled` is set via
/// [`Gateway::enable_embed`]; the [`Embedder`](holofs_embed::Embedder)
/// itself is constructed on the first PUT or query after that, so a
/// server that never gets asked to embed pays nothing.
///
/// `pub(crate)` because the outer `Gateway` struct still owns an
/// `Arc<Mutex<EmbedState>>` field, and both the GC path (which
/// rewrites embeddings.bin) and the versions path (which invalidates
/// the cached ANN when a manifest is dropped) reach into this state.
#[derive(Default)]
pub(crate) struct EmbedState {
    pub(crate) enabled: bool,
    pub(crate) index_path: Option<std::path::PathBuf>,
    pub(crate) embedder: Option<Arc<holofs_embed::Embedder>>,
    /// in-memory ANN index. `None` until the first
    /// `semantic_search` after a PUT (or after a startup) — then
    /// built from the entire `embeddings.bin`. Bumped to `None` by
    /// `ann_generation` mismatches so the next query rebuilds.
    pub(crate) ann: Option<Arc<holofs_embed::HnswIndex>>,
    /// Increments every time a new embedding is appended. Compared
    /// against the generation the cached `ann` was built at — when
    /// they diverge we drop the cache and rebuild.
    pub(crate) ann_generation: u64,
    /// Generation `ann` was built at. `None` until first build.
    pub(crate) ann_built_at: Option<u64>,
}

/// Layer-band selector for [`Gateway::semantic_search`]. `Any` (the
/// default) takes the best score across all bands per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBand {
    /// L0 reconstruction — silhouette / colour blob.
    Coarse,
    /// L0-L2 reconstruction — silhouette + low-freq detail.
    Mid,
    /// All layers — texture / fine detail.
    Full,
    /// Search all three bands and keep the best score per file.
    Any,
}

impl SearchBand {
    /// Parse from URL string (`coarse` / `mid` / `full` / `any`).
    /// Unknown / empty values map to `Any`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "coarse" | "structure" => Self::Coarse,
            "mid" => Self::Mid,
            "full" | "texture" | "detail" => Self::Full,
            _ => Self::Any,
        }
    }
}

/// One row of [`Gateway::semantic_search`] output. The gateway returns
/// catalog names + scores; the frontend renders them as a card grid.
#[derive(Debug, Clone)]
pub struct SemanticHit {
    /// Catalog name of the matching object.
    pub name: String,
    /// Cosine similarity to the query in `[-1.0, 1.0]`. CLIP-base
    /// scores cluster narrowly around 0.2-0.35 even for strong matches,
    /// so the UI usually shows them as 0..100 percentiles instead of
    /// raw values.
    pub score: f32,
    /// which layer band produced the winning score. For
    /// queries filtered to one band this is always that band; for
    /// `SearchBand::Any` it's whichever of the three scored best.
    pub band: SearchBand,
}

impl Gateway {
    /// Lazily build the [`Embedder`](holofs_embed::Embedder) handle.
    /// The first call downloads ~155 MiB of CLIP weights from
    /// HuggingFace into `~/.cache/huggingface/hub`; the next process
    /// boot reads from the cache in milliseconds. Returns `None` when
    /// the embed feature is disabled.
    pub(crate) async fn ensure_embedder(
        &self,
    ) -> Result<Option<Arc<holofs_embed::Embedder>>, GatewayError> {
        // Fast path — already initialised.
        {
            let s = self.embed.lock().await;
            if !s.enabled {
                return Ok(None);
            }
            if let Some(e) = &s.embedder {
                return Ok(Some(Arc::clone(e)));
            }
        }
        // Slow path: spawn_blocking around the candle init.
        let built = tokio::task::spawn_blocking(holofs_embed::Embedder::new)
            .await
            .map_err(|e| GatewayError::BadRequest(format!("embed init join: {e}")))?
            .map_err(|e| GatewayError::BadRequest(format!("embed init: {e}")))?;
        let arc = Arc::new(built);
        let mut s = self.embed.lock().await;
        // Another task may have raced us.
        if let Some(e) = &s.embedder {
            return Ok(Some(Arc::clone(e)));
        }
        s.embedder = Some(Arc::clone(&arc));
        Ok(Some(arc))
    }

    /// Decode the coarse layers of an image-kind object, run CLIP, and
    /// append the embedding to the on-disk index. Idempotent —
    /// `data_cid` re-uploads / duplicates are skipped via
    /// `Index::has`. Returns:
    ///   * `Ok(true)` — newly embedded,
    ///   * `Ok(false)` — already in the index (or embed disabled, or
    ///     wrong kind),
    ///   * `Err(...)` — decode / inference failure.
    pub async fn embed_object(&self, name: &str) -> Result<bool, GatewayError> {
        // epoch-GC: `gc_barrier` was narrowed to just the
        // embeddings.bin path. GC's tail rewrites embeddings.bin
        // under `gc_barrier.write`; our append (also a write to
        // that file) takes the read guard so the two never
        // interleave.
        let _gc_guard = self.gc_barrier.read().await;
        let Some(embedder) = self.ensure_embedder().await? else {
            return Ok(false);
        };
        let index_path = match self.embed.lock().await.index_path.clone() {
            Some(p) => p,
            None => return Ok(false),
        };

        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        // only embeds images. Audio / text get their own
        // embedding pipeline in a future stage.
        if manifest.kind != ObjectKind::Image {
            return Ok(false);
        }

        // embed three layer bands per image so the
        // /search page can route queries by abstraction level.
        // Coarse = L0 (silhouette / colour blob), Mid = L0-L2
        // (silhouette + low-freq detail), Full = all layers
        // (full-resolution texture). Each band lives as a distinct
        // record in `embeddings.bin` keyed by (data_cid, band) and is
        // independently dedup-able — re-running embed_object on an
        // already-indexed file is cheap because every band short-
        // circuits at the `Index::has` check.
        let data_cid = manifest.data_cid;
        let last_layer = manifest.nlayers.saturating_sub(1);
        let bands: &[(holofs_embed::LayerBand, u8)] = &[
            (holofs_embed::LayerBand::Coarse, 0),
            (holofs_embed::LayerBand::Mid, 2u8.min(last_layer)),
            (holofs_embed::LayerBand::Full, last_layer),
        ];

        let mut any_new = false;
        let live = self.effective_live().await;
        let _ = live; // reserved for future band-specific gather diagnostics
        let width = manifest.width;
        let height = manifest.height;
        let n = (width as usize) * (height as usize);

        for &(band, max_layer) in bands {
            // Per-band dedup check first to skip the decode pass.
            let already = {
                let idx_path = index_path.clone();
                tokio::task::spawn_blocking(move || -> Result<bool, GatewayError> {
                    let idx = holofs_embed::Index::open(&idx_path)
                        .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
                    idx.has(&data_cid, band)
                        .map_err(|e| GatewayError::BadRequest(format!("embed has: {e}")))
                })
                .await
                .map_err(|e| GatewayError::BadRequest(format!("embed has join: {e}")))??
            };
            if already {
                continue;
            }

            let (channels, _bytes_dl) = self
                .decode_with_autorepair(name, max_layer)
                .await
                .map_err(|e| GatewayError::Decode(format!("embed decode: {e}")))?;
            if channels.len() < 3 || channels.iter().any(|c| c.len() != n) {
                return Err(GatewayError::Decode(
                    "embed decode: unexpected channel shape".into(),
                ));
            }
            let mut rgb = vec![0u8; 3 * n];
            for i in 0..n {
                rgb[3 * i] = channels[0][i].clamp(0.0, 255.0).round() as u8;
                rgb[3 * i + 1] = channels[1][i].clamp(0.0, 255.0).round() as u8;
                rgb[3 * i + 2] = channels[2][i].clamp(0.0, 255.0).round() as u8;
            }
            let name_owned = name.to_string();
            let embedder_h = Arc::clone(&embedder);
            let idx_path = index_path.clone();
            tokio::task::spawn_blocking(move || -> Result<(), GatewayError> {
                let vec = embedder_h
                    .embed_image(&rgb, width, height)
                    .map_err(|e| GatewayError::Decode(format!("clip image: {e}")))?;
                let idx = holofs_embed::Index::open(&idx_path)
                    .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
                let rec = holofs_embed::EmbedRecord {
                    data_cid,
                    band,
                    name: name_owned,
                    vec,
                };
                idx.append(&rec)
                    .map_err(|e| GatewayError::BadRequest(format!("embed append: {e}")))?;
                Ok(())
            })
            .await
            .map_err(|e| GatewayError::BadRequest(format!("embed join: {e}")))??;
            any_new = true;
        }
        if any_new {
            // bump the ANN generation so the next
            // semantic_search call rebuilds (or — for small bands —
            // re-loads the in-memory record vec). The rebuild itself
            // is lazy; we just signal staleness here.
            self.embed.lock().await.ann_generation += 1;
        }
        Ok(any_new)
    }

    /// Semantic search backed by [`holofs_embed::HnswIndex`].
    ///
    /// previously a brute-force flat scan over
    /// `embeddings.bin` on every query. Now lazily builds an
    /// in-memory ANN index, cached across queries until the next
    /// PUT bumps `ann_generation`. Small bands still fall back to
    /// brute-force inside the index (cheap and lower-latency under
    /// a few hundred vectors); larger bands graduate to HNSW. The
    /// public contract is identical — same SemanticHit shape, same
    /// "best-band-per-name when band == Any" semantics.
    pub async fn semantic_search(
        &self,
        query: &str,
        limit: usize,
        band: SearchBand,
    ) -> Result<Vec<SemanticHit>, GatewayError> {
        let Some(embedder) = self.ensure_embedder().await? else {
            return Ok(Vec::new());
        };
        let ann = match self.ensure_ann_index().await? {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };
        let query = query.to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<SemanticHit>, GatewayError> {
            let q = embedder
                .embed_text(&query)
                .map_err(|e| GatewayError::Decode(format!("clip text: {e}")))?;
            let raw = match band {
                SearchBand::Any => ann.search_any(&q, limit),
                other => {
                    let band_enum = match other {
                        SearchBand::Coarse => holofs_embed::LayerBand::Coarse,
                        SearchBand::Mid => holofs_embed::LayerBand::Mid,
                        SearchBand::Full => holofs_embed::LayerBand::Full,
                        SearchBand::Any => unreachable!(),
                    };
                    ann.search(band_enum, &q, limit)
                }
            };
            let hits: Vec<SemanticHit> = raw
                .into_iter()
                .map(|h| SemanticHit {
                    name: h.name,
                    score: h.score,
                    band: match h.band {
                        holofs_embed::LayerBand::Coarse => SearchBand::Coarse,
                        holofs_embed::LayerBand::Mid => SearchBand::Mid,
                        holofs_embed::LayerBand::Full => SearchBand::Full,
                    },
                })
                .collect();
            Ok(hits)
        })
        .await
        .map_err(|e| GatewayError::BadRequest(format!("search join: {e}")))?;
        result
    }

    /// Lazily build (or reuse) the in-memory ANN index. Rebuilds the
    /// whole index when the cached generation is stale relative to
    /// `ann_generation`; otherwise returns the cached `Arc` directly.
    /// Returns `Ok(None)` when the embed feature is disabled or the
    /// `embeddings.bin` path is unset.
    async fn ensure_ann_index(
        &self,
    ) -> Result<Option<Arc<holofs_embed::HnswIndex>>, GatewayError> {
        // Fast path.
        {
            let s = self.embed.lock().await;
            if !s.enabled {
                return Ok(None);
            }
            if let (Some(ann), Some(built_at)) = (&s.ann, s.ann_built_at) {
                if built_at == s.ann_generation {
                    return Ok(Some(Arc::clone(ann)));
                }
            }
        }
        // Slow path — rebuild. Snapshot path + generation, drop the
        // lock, read records, build HNSW off-thread, then store back.
        let (index_path, generation) = {
            let s = self.embed.lock().await;
            let p = match &s.index_path {
                Some(p) => p.clone(),
                None => return Ok(None),
            };
            (p, s.ann_generation)
        };
        let built = tokio::task::spawn_blocking(move || -> Result<holofs_embed::HnswIndex, GatewayError> {
            let idx = holofs_embed::Index::open(&index_path)
                .map_err(|e| GatewayError::BadRequest(format!("embed index: {e}")))?;
            let mut recs: Vec<holofs_embed::EmbedRecord> = Vec::new();
            for r in idx
                .iter()
                .map_err(|e| GatewayError::BadRequest(format!("embed iter: {e}")))?
            {
                let r = r.map_err(|e| GatewayError::BadRequest(format!("embed rec: {e}")))?;
                if !r.vec.is_empty() {
                    recs.push(r);
                }
            }
            Ok(holofs_embed::HnswIndex::build_from(recs))
        })
        .await
        .map_err(|e| GatewayError::BadRequest(format!("ann build join: {e}")))??;
        let arc = Arc::new(built);
        let mut s = self.embed.lock().await;
        // Another task may have raced us with a *newer* generation;
        // if so we still write ours — the next query will rebuild
        // again, which is fine. The point of the cache is amortising
        // across many queries between writes, not strict freshness.
        if s.ann_generation == generation {
            s.ann = Some(Arc::clone(&arc));
            s.ann_built_at = Some(generation);
        }
        Ok(Some(arc))
    }

    /// Fire-and-forget embedding for a freshly-PUT object. Called
    /// from the web / MCP ingest handlers right after `ingest_bytes`
    /// succeeds. Returns immediately; failures land in stderr. No-op
    /// when the embed feature is disabled.
    pub fn embed_object_in_background(self: &Arc<Self>, name: String) {
        let gw = Arc::clone(self);
        tokio::spawn(async move {
            if !gw.embed_enabled().await {
                return;
            }
            if let Err(e) = gw.embed_object(&name).await {
                eprintln!("embed bg {name}: {e}");
            }
        });
    }

    /// Walk the catalog and embed every image that isn't in the index
    /// yet. Returns `(newly_embedded, skipped)`. Used by the
    /// `holofs embed-all` CLI command.
    pub async fn embed_all_pending(&self) -> Result<(usize, usize), GatewayError> {
        let names: Vec<String> = {
            let cat = self.catalog.lock().await;
            cat.entries
                .iter()
                .filter_map(|(name, m)| {
                    if m.kind == ObjectKind::Image {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let mut new_n = 0;
        let mut skip_n = 0;
        for name in names {
            match self.embed_object(&name).await {
                Ok(true) => new_n += 1,
                Ok(false) => skip_n += 1,
                Err(e) => {
                    // Continue on per-file failure — one broken object
                    // shouldn't stall the whole catalog index.
                    eprintln!("embed {name}: {e}");
                }
            }
        }
        Ok((new_n, skip_n))
    }
}
