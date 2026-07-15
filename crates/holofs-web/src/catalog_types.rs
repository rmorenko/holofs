//! Shared catalog view-models carried over the wire between the
//! `#[server]` functions and the Leptos components that render them.
//!
//! Kept in its own file so both compilation targets (`ssr` — server
//! render + `handle_server_fns_with_context` dispatch — and `hydrate`
//! — WASM bundle in the browser) see the same struct definition
//! without conditional compilation gymnastics. `from_manifest` is
//! `ssr`-only because it depends on `holofs_model::manifest::Manifest`
//! which does not compile to WASM.
//!

use serde::{Deserialize, Serialize};

/// Catalog view-model carried over the wire by the [`crate::get_catalog`]
/// server function. Stays plain-serde so both SSR and hydrate compile it
/// cleanly (no tokio / no holofs-gateway dependency).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Full catalog path (`photos/2026/img.png`).
    pub name: String,
    /// One of `"image"`, `"audio"`, `"text"`, `"opaque"`, `"directory"`.
    pub kind: String,
    /// MIME type from the manifest.
    pub content_type: String,
    /// Image width in pixels; `0` for non-image kinds.
    pub width: u32,
    /// Image height in pixels; `0` for non-image kinds.
    pub height: u32,
    /// Total planned shards across every channel × layer.
    pub n_shards: u32,
    /// Approximate on-cluster storage in bytes (`Σ n_per_layer × (sym_len + K) × channels`).
    /// A hygiene · UI-UX fix — the card used to show "N shards" which
    /// nobody except the ops engineer could turn into an intuition of
    /// "how big is this file". Format-as-KB/MB in the UI.
    pub bytes_stored: u64,
    /// First 12 hex chars of `data_cid` — used as a short display id.
    pub cid_short: String,
    /// Audio sample rate; `0` for non-audio kinds.
    pub audio_sample_rate: u32,
    /// Number of channels (audio) or `3` (RGB image) / `1` (text, opaque).
    pub channels: u8,
    /// Unix epoch seconds when this entry was added to the catalog. `0`
    /// means "unknown" — typically a legacy manifest written under
    /// `HOLOFSM6` or `HOLOFSM7`, which had no timestamp.
    pub created_at_unix: u64,
}

#[cfg(feature = "ssr")]
impl CatalogEntry {
    /// Project a `Manifest` into the wire-friendly view-model.
    pub(crate) fn from_manifest(name: &str, m: &holofs_model::manifest::Manifest) -> Self {
        use holofs_core::hash::hex;
        use holofs_model::manifest::ObjectKind;

        let kind = match m.kind {
            ObjectKind::Image => "image",
            ObjectKind::Audio => "audio",
            ObjectKind::Text => "text",
            ObjectKind::Opaque => "opaque",
            ObjectKind::Directory => "directory",
        }
        .to_string();
        let total_shards: u32 = m.n_per_layer.iter().sum::<u32>() * u32::from(m.channels);
        // Same shape as api_stats::bytes_total: for each layer,
        // n_shards × payload_bytes (sym_len + K coeff bytes) × channels.
        let bytes_stored: u64 = m
            .n_per_layer
            .iter()
            .enumerate()
            .map(|(l, npl)| {
                let sym = m.sym_len.get(l).copied().unwrap_or(0) as u64;
                let per_shard = sym + m.k as u64;
                u64::from(*npl) * per_shard * u64::from(m.channels)
            })
            .sum();
        let cid_full = hex(&m.data_cid);
        Self {
            name: name.to_string(),
            kind,
            content_type: m.content_type.clone(),
            width: m.width,
            height: m.height,
            n_shards: total_shards,
            bytes_stored,
            cid_short: cid_full.chars().take(12).collect(),
            audio_sample_rate: m.audio_sample_rate,
            channels: m.channels,
            created_at_unix: m.created_at_unix,
        }
    }
}
