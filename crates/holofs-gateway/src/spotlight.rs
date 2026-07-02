//! Stage 13.2 / 14.1 — holographic spotlight.
//!
//! Two composite modes over the same ROI abstraction:
//!
//! - `spotlight` (Stage 13.2): decode L0 + full quality, blit
//!   the sharp full-quality pixels inside the ROI over an L0
//!   background. Outside the ROI stays blurry-but-visible.
//! - `spotlight_coeff` (Stage 14.1): decode every layer but mask
//!   the DWT coefficients outside the ROI's Haar reverse map.
//!   Non-ROI pixels collapse to black; the ROI is sharp and the
//!   composite lives entirely in coefficient space.
//!
//! Moved out of `http_gateway.rs` in Phase R1b.7.

use std::time::Instant;

use holofs_client::{get_object_blocks, get_object_up_to_layer, get_object_with_coeff_mask};
use holofs_core::transform::roi_to_block_ids_with_stride;
use holofs_model::manifest::{ObjectEncoding, ObjectKind};

use crate::error::GatewayError;
use crate::util::encode_png;
use crate::Gateway;

/// Stage 13.2: region-of-interest for [`Gateway::spotlight`]. All four
/// coordinates are normalised image-relative (`0.0..=1.0`). `x`/`y` is
/// the top-left corner; `w`/`h` is the rectangle's extent.
#[derive(Debug, Clone, Copy)]
pub struct SpotlightRoi {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Result of [`Gateway::spotlight`].
#[derive(Debug, Clone)]
pub struct SpotlightImage {
    /// PNG bytes of the composited image: full-quality inside the ROI,
    /// L0 (coarse) blur outside.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub nlayers: u8,
    /// Sum of both decode passes' bandwidth.
    pub bytes_downloaded: u64,
    pub decode_ms: u128,
    /// `(x, y, w, h)` of the ROI in actual pixels after clamping —
    /// echoed back so the UI can draw a frame on top of the image.
    pub roi_px: (u32, u32, u32, u32),
}


impl Gateway {

    /// Stage 13.2: holographic spotlight — decode the image twice (L0
    /// only and full quality), then composite per-pixel so the rectangle
    /// `(x_pct, y_pct, w_pct, h_pct)` inside the image is sharp while
    /// everything else stays at L0 blur. The architectural pitch from
    /// `/about` made concrete: detail layers selectively rendered to
    /// the spatial region the user cares about, no re-encoding of
    /// anything.
    ///
    /// `roi` is normalised: each coordinate is `0.0..=1.0` of the image
    /// width / height. Clamped to image bounds.
    pub async fn spotlight(
        &self,
        name: &str,
        roi: SpotlightRoi,
    ) -> Result<SpotlightImage, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "spotlight: not applicable to non-image objects".into(),
            ));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let max_layer = manifest.nlayers.saturating_sub(1);

        // Coarse pass (L0). Cheap — typically a tenth of the bytes of
        // the full decode.
        let (lo_channels, lo_bytes) = self
            .decode_with_autorepair(name, 0)
            .await
            .map_err(|e| GatewayError::Decode(format!("spotlight L0: {e}")))?;
        // Full pass.
        let (hi_channels, hi_bytes) = self
            .decode_with_autorepair(name, max_layer)
            .await
            .map_err(|e| GatewayError::Decode(format!("spotlight full: {e}")))?;

        let w = manifest.width as usize;
        let h = manifest.height as usize;
        let n_pixels = w * h;
        if lo_channels.len() != hi_channels.len()
            || lo_channels.iter().any(|c| c.len() != n_pixels)
            || hi_channels.iter().any(|c| c.len() != n_pixels)
        {
            return Err(GatewayError::Decode(
                "spotlight: channel shape mismatch".into(),
            ));
        }
        // Translate normalised ROI to pixel coords, clamped to image
        // bounds. `roi.w == 0 || roi.h == 0` produces an all-blurry
        // image — same behaviour as "no spotlight requested".
        let x0 = (roi.x.clamp(0.0, 1.0) * w as f32).round() as usize;
        let y0 = (roi.y.clamp(0.0, 1.0) * h as f32).round() as usize;
        let x1 = ((roi.x + roi.w).clamp(0.0, 1.0) * w as f32).round() as usize;
        let y1 = ((roi.y + roi.h).clamp(0.0, 1.0) * h as f32).round() as usize;
        let n_channels = lo_channels.len();
        let mut composed: Vec<Vec<f32>> = vec![Vec::with_capacity(n_pixels); n_channels];
        for c in 0..n_channels {
            composed[c].resize(n_pixels, 0.0);
            for y in 0..h {
                let inside_y = y >= y0 && y < y1;
                let row_off = y * w;
                for x in 0..w {
                    let inside = inside_y && x >= x0 && x < x1;
                    let idx = row_off + x;
                    composed[c][idx] = if inside {
                        hi_channels[c][idx]
                    } else {
                        lo_channels[c][idx]
                    };
                }
            }
        }
        let png = encode_png(&composed, manifest.width, manifest.height);
        Ok(SpotlightImage {
            bytes: png,
            width: manifest.width,
            height: manifest.height,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            bytes_downloaded: lo_bytes + hi_bytes,
            decode_ms: t0.elapsed().as_millis(),
            roi_px: (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32),
        })
    }

    /// Stage 14.1: "coefficient-mask" spotlight — alternative to
    /// [`Self::spotlight`]'s spatial composite.
    ///
    /// Maps the spatial ROI to the set of DWT-plane positions whose
    /// coefficient affects ROI pixels (via the Haar reverse map in
    /// `holofs_core::transform`), then decodes the whole image but
    /// places only those coefficients into the reconstruction plane
    /// before the inverse Haar. Non-ROI pixels collapse to black.
    ///
    /// Visual difference vs Stage 13.2:
    ///   * Stage 13.2 (`spotlight`) = decode coarse + full, composite
    ///     per pixel. Outside ROI stays blurry-but-visible.
    ///   * Stage 14.1 (`spotlight_coeff`) = decode every layer, mask
    ///     coefficients outside ROI. Outside ROI is black (or near-
    ///     black, since Haar with masked high coefficients leaks a
    ///     little).
    ///
    /// For **RLNC** objects: same bandwidth as a full fetch (RLNC
    /// mixes every coefficient across every shard). The win is
    /// spatial.
    ///
    /// For **Replicated** objects (Stage 15.1): the ROI's Haar
    /// reverse-map is converted to per-layer block ids via
    /// `roi_to_block_ids_with_stride`; only those blocks are
    /// fetched. Bandwidth scales linearly with the ROI area — the
    /// marquee "bandwidth-aware spotlight" the Stage 14.1 mask
    /// primitive predicted but couldn't deliver on RLNC.
    pub async fn spotlight_coeff(
        &self,
        name: &str,
        roi: SpotlightRoi,
    ) -> Result<SpotlightImage, GatewayError> {
        let manifest = self
            .catalog
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or(GatewayError::NotFound)?;
        if manifest.kind != ObjectKind::Image {
            return Err(GatewayError::BadRequest(
                "spotlight_coeff: not applicable to non-image objects".into(),
            ));
        }
        let w = manifest.width as usize;
        let h = manifest.height as usize;
        let levels = manifest.levels as usize;
        let x0 = (roi.x.clamp(0.0, 1.0) * w as f32).round() as usize;
        let y0 = (roi.y.clamp(0.0, 1.0) * h as f32).round() as usize;
        let x1 = ((roi.x + roi.w).clamp(0.0, 1.0) * w as f32).round() as usize;
        let y1 = ((roi.y + roi.h).clamp(0.0, 1.0) * h as f32).round() as usize;
        if x1 <= x0 || y1 <= y0 {
            return Err(GatewayError::BadRequest(
                "spotlight_coeff: empty ROI".into(),
            ));
        }
        let live = self.effective_live().await;
        let t0 = Instant::now();
        let (channels, bytes_dl) = match manifest.encoding {
            ObjectEncoding::Replicated { block_size, .. } => {
                // Stage 15.1: fetch only the blocks whose
                // coefficients affect the ROI. Positions outside
                // the touched set stay at 0 → decode-mask
                // semantics fall out for free (haar_inverse of
                // zeros is zeros; only ROI pixels get non-zero
                // reconstruction).
                let ids = roi_to_block_ids_with_stride(
                    x0,
                    y0,
                    x1 - x0,
                    y1 - y0,
                    w,
                    h,
                    &manifest.layer_positions,
                    block_size as usize,
                );
                get_object_blocks(&manifest, &live, &ids)
                    .await
                    .map_err(|e| {
                        GatewayError::Decode(format!("spotlight_coeff blocks: {e}"))
                    })?
            }
            ObjectEncoding::Rlnc => {
                let positions = holofs_core::transform::spatial_to_dwt_positions(
                    x0,
                    y0,
                    x1 - x0,
                    y1 - y0,
                    w,
                    h,
                    levels,
                );
                let allowed: std::collections::HashSet<usize> =
                    positions.into_iter().collect();
                get_object_with_coeff_mask(&self.gf, &manifest, &live, &allowed)
                    .await
                    .map_err(|e| GatewayError::Decode(format!("spotlight_coeff: {e}")))?
            }
        };
        let decode_ms = t0.elapsed().as_millis();
        let png = encode_png(&channels, manifest.width, manifest.height);
        Ok(SpotlightImage {
            bytes: png,
            width: manifest.width,
            height: manifest.height,
            channels: manifest.channels,
            nlayers: manifest.nlayers,
            bytes_downloaded: bytes_dl,
            decode_ms,
            roi_px: (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32),
        })
    }
}
