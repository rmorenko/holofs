//! Axum handlers for the holofs-web HTTP surface — server-only.
//!
//! Every handler wraps a public method on [`holofs_gateway::Gateway`]
//! and shapes the result into `axum::response::Response` with the
//! `X-Holofs-*` header vocabulary documented in
//! [`docs/api.md`](../../../docs/api.md).
//!
//! The historical single-file `handlers.rs` (1857 lines) was split
//! into domain modules during //!
//! | Submodule            | Handlers                                            |
//! |----------------------|-----------------------------------------------------|
//! | [`objects`]          | GET / PUT / DELETE `/*path`, `/preview/*`,          |
//! |                      | `/preview/stream/*`, `/api/shard/…`, wasm alias.    |
//! | [`dirops`]           | mkdir, rmdir, rm, mv (JSON + form flavours).        |
//! | [`uploads`]          | multipart `/api/upload`.                            |
//! | [`versions`]         | `/api/restore`, `/api/versions/delete`.             |
//! | [`analytics`]        | `/api/fingerprint/*`, `/api/mix.png`,                |
//! |                      | `/api/mix-save`, `/api/spotlight.png`.                |
//! | [`search`]           | `/api/embed_all`, `/api/search`.                    |
//! | [`health`]           | `/api/stats`, `/metrics`, `/api/gc`,                 |
//! |                      | `/admin/node`, `/api/health/events` SSE.            |
//! | [`escrow`]           | `/escrow/split`, `/escrow/download`, `/escrow/recover`.|
//! | [`util`]             | Pure helpers — path validation, form parsing,        |
//! |                      | HTML/JSON escape, header shortcuts, `error_to_response`.|
//! | [`response`]         | Response builders — `serve_with_range`, ingest /     |
//! |                      | remove / mkdir / rmdir / rename → HTTP, stats +      |
//! |                      | fingerprint → JSON.                                  |
//!
//! Every handler is re-exported at this module's root so
//! `holofs_web::handlers::foo` keeps resolving from `main.rs`.

#![cfg(feature = "ssr")]

// R2b.2: pure helpers + response builders are shared across every
// domain submodule. Kept `pub(crate)` so the submodules can `use
// super::util::...` and `use super::response::...`. Nothing outside
// the crate reads these directly.
pub(crate) mod response;
pub(crate) mod util;

// Handler domain modules (R2b.1 through R2b.5). The `pub use` block
// after each `mod` keeps the historical `handlers::foo` paths in
// `main.rs` resolving unchanged.
mod analytics;
mod dirops;
mod escrow;
mod health;
mod objects;
mod search;
mod uploads;
mod versions;

pub use analytics::{api_fingerprint, mix_preview, mix_save, spotlight_png};
pub use dirops::{batch_delete, mkdir, mkdir_form, mv, rm_form, rmdir, rmdir_form};
pub use escrow::{escrow_download, escrow_recover, escrow_split};
pub use health::{
    admin_add_node, admin_catalog_names, api_stats, gc_orphans, health_events, metrics,
    toggle_node,
};
pub use objects::{
    delete_object, get_object, get_preview, get_shard_png, preview_stream, put_object,
    serve_wasm_alias,
};
pub use search::{embed_all, semantic_search};
pub use uploads::upload_form;
pub use versions::{delete_version_form, restore_version_form};
