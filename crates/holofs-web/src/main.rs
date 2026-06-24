//! holofs-web SSR binary.
//!
//! Boots axum + leptos_axum on `--addr` (or `LEPTOS_SITE_ADDR`). Cluster
//! bootstrap (nodes, catalog, gateway, monitor, auditor) lives in
//! [`holofs_web::bootstrap::bootstrap_cluster`]. Logging, CLI parsing and
//! metrics scaffolding are wired here so the binary is observable in
//! production-shaped environments.
//!
//! Static assets (WASM bundle + anything under `assets/`) live under
//! `target/site/` and are produced by `cargo leptos build`.

#![cfg(feature = "ssr")]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Request, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Router};
use clap::Parser;
use leptos::prelude::*;
use leptos_axum::{generate_route_list, handle_server_fns_with_context, LeptosRoutes};
use tower_http::services::ServeDir;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::{info, Level};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use holofs_web::bootstrap::{bootstrap_cluster, Bootstrap};
use holofs_web::cli::{Cli, LogFormat};
use holofs_web::diff::GetDiff;
use holofs_web::handlers;
use holofs_web::health::{GetHealthIndex, GetObjectHealth};
use holofs_web::inspect::{GetInspect, GetInspectZoom};
use holofs_web::help::{GetDoc, ListDocs};
use holofs_web::similar::GetSimilar;
use holofs_web::{App, GetCatalog, ListDir, ListDirPageFn};

/// Per-request body cap for upload routes (PUT and the two `/escrow/*`
/// multipart endpoints). axum's default is 2 MiB which rejects realistic
/// media uploads with a misleading "multipart parsing" error. 256 MiB
/// covers typical PNG / WAV / video chunks; the ceiling protects against
/// runaway requests on a shared deployment.
const UPLOAD_BODY_LIMIT: usize = 256 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    init_tracing(&cli);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        addr = %cli.addr,
        storage = %cli.storage.display(),
        log_format = ?cli.log_format,
        "starting holofs-web"
    );

    let Bootstrap {
        gateway,
        monitor: _monitor,
        auditor: _auditor,
    } = bootstrap_cluster(&cli.bootstrap_config())
        .await
        .expect("cluster bootstrap failed");

    // Leptos config — read from `[package.metadata.leptos]` in Cargo.toml.
    // Leptos 0.7: `get_configuration` is now sync (no `.await`).
    let conf = get_configuration(Some("crates/holofs-web/Cargo.toml"))
        .expect("failed to read leptos config");
    // The CLI/env LEPTOS_SITE_ADDR is the source of truth for the bind
    // address; we override whatever cargo-leptos baked into the config.
    let mut leptos_options = conf.leptos_options;
    leptos_options.site_addr = cli.addr;
    let site_root = leptos_options.site_root.clone();

    // Explicit server_fn registration (inventory fails on Apple Silicon).
    server_fn::axum::register_explicit::<GetCatalog>();
    server_fn::axum::register_explicit::<ListDir>();
    server_fn::axum::register_explicit::<ListDirPageFn>();
    server_fn::axum::register_explicit::<GetHealthIndex>();
    server_fn::axum::register_explicit::<GetObjectHealth>();
    server_fn::axum::register_explicit::<GetInspect>();
    server_fn::axum::register_explicit::<GetInspectZoom>();
    server_fn::axum::register_explicit::<GetSimilar>();
    server_fn::axum::register_explicit::<GetDiff>();
    // Stage 10: in-app help / docs viewer.
    server_fn::axum::register_explicit::<GetDoc>();
    server_fn::axum::register_explicit::<ListDocs>();

    let routes = generate_route_list(App);

    // Two clones — one for the leptos route renderer, one for the server-fn
    // handler. Both need the same `Arc<Gateway>` in context.
    let gw_for_routes = Arc::clone(&gateway);
    let gw_for_server_fns = Arc::clone(&gateway);

    let app = Router::new()
        .route(
            "/api/*fn_name",
            post(move |req: Request<Body>| {
                let gw = Arc::clone(&gw_for_server_fns);
                async move {
                    handle_server_fns_with_context(
                        move || {
                            provide_context(Arc::clone(&gw));
                        },
                        req,
                    )
                    .await
                }
            }),
        )
        // JSON / SSE / admin / escrow routes (Phases 4b.3–4c).
        .route("/api/stats", get(handlers::api_stats))
        .route("/api/fingerprint/*path", get(handlers::api_fingerprint))
        .route("/api/health/events", get(handlers::health_events))
        // Stage 9: directory operations. Two flavours per op — the
        // path-wildcard JSON variants for API/curl users, plus a
        // form-urlencoded POST that the catalog page's HTML forms can hit
        // without JavaScript. The form variants 303-redirect on success.
        .route("/api/mkdir/*path", post(handlers::mkdir))
        .route("/api/mkdir", post(handlers::mkdir_form))
        .route("/api/rmdir/*path", delete(handlers::rmdir))
        .route("/api/rmdir", post(handlers::rmdir_form))
        // Stage 11.17: form-friendly file delete (mirror of rmdir_form
        // for non-directory entries).
        .route("/api/rm", post(handlers::rm_form))
        .route("/api/mv", post(handlers::mv))
        // Stage 11.4: form-friendly file upload from the catalog page.
        .route(
            "/api/upload",
            post(handlers::upload_form).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        // Stage 5: Prometheus exposition endpoint.
        .route("/metrics", get(handlers::metrics))
        .route("/admin/node", post(handlers::toggle_node))
        // Stage 11.3: bypass axum's 2 MiB default body limit for routes
        // that accept media uploads. Realistic photos/audio land in the
        // 5–80 MB range; the default emitted a misleading "multipart
        // parsing" 400 because the body was truncated mid-parse.
        .route(
            "/escrow/split",
            post(handlers::escrow_split).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/escrow/recover",
            post(handlers::escrow_recover).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/escrow/download/:path", get(handlers::escrow_download))
        // Stage 9: c_l_idx leads so the wildcard can capture multi-segment
        // object paths after it.
        .route(
            "/api/shard/:c_l_idx/*path",
            get(handlers::get_shard_png),
        )
        .route("/preview/*path", get(handlers::get_preview))
        .route("/*path", get(handlers::get_object))
        .route(
            "/*path",
            put(handlers::put_object).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/*path", delete(handlers::delete_object))
        // Stage 12.0 + 12.1: MCP (Model Context Protocol) server over
        // Streamable HTTP. The tower service handles POST/GET/DELETE on
        // `/mcp` per the spec — wire it as `nest_service` so axum hands
        // the whole sub-path off to rmcp instead of routing per-method.
        // Tools share the cluster's live `Arc<Gateway>`, so MCP clients
        // see the same catalog as the UI.
        //
        // If `HOLOFS_MCP_TOKEN` is set, mount a bearer-auth middleware
        // in front and flip `writes_enabled=true`; without the env var
        // the endpoint stays open but read-only.
        .merge(build_mcp_router(Arc::clone(&gateway)))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            move || {
                provide_context(Arc::clone(&gw_for_routes));
            },
            App,
        )
        .nest_service("/pkg", ServeDir::new(format!("{site_root}/pkg")))
        // Stage 11.5: static assets used by the upload form, the help
        // viewer (Mermaid + KaTeX bootstrap), and anything else dropped
        // into `crates/holofs-web/assets/`.
        //
        // Stage 11.14: `ServeDir` falls back to the source directory if
        // `target/site/assets/` is missing — `cargo leptos build` does
        // not copy the source assets-dir reliably across rebuilds, and
        // we don't want help-init.js / mermaid bootstrap to 404 just
        // because the user only ran `cargo build`. Production
        // deployments (Docker / Helm) bundle assets at the
        // `target/site/assets` path so the primary `ServeDir` wins.
        .nest_service(
            "/assets",
            ServeDir::new(format!("{site_root}/assets"))
                .fallback(ServeDir::new("crates/holofs-web/assets")),
        )
        .fallback(fallback)
        .layer(Extension(Arc::clone(&gateway)))
        // tower-http TraceLayer turns each HTTP request into a tracing span:
        // method, path, status code, latency. Pairs with --log-format=json
        // for ELK-friendly request logs.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(leptos_options);

    let addr = cli.addr;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    axum::serve(listener, app.into_make_service())
        .await
        .expect("axum::serve");
}

/// Initialise the tracing subscriber based on `--log` and `--log-format`.
/// JSON format is meant for structured log shippers (journald, fluentd,
/// Vector); text is ANSI-coloured when stdout is a TTY.
fn init_tracing(cli: &Cli) {
    let filter = EnvFilter::try_new(&cli.log)
        .unwrap_or_else(|_| EnvFilter::new("info,holofs_web=debug"));
    match cli.log_format {
        LogFormat::Json => {
            let layer = fmt::layer().json().with_target(true);
            tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .init();
        }
        LogFormat::Text => {
            let layer = fmt::layer()
                .with_ansi(true)
                .with_target(true)
                .with_thread_ids(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .init();
        }
    }
}

/// Fallback for paths the Leptos router does not match: serve a static file
/// from the site root, otherwise let the App render (404 view).
async fn fallback(
    uri: Uri,
    State(options): State<LeptosOptions>,
    req: Request<Body>,
) -> Response {
    use tokio::fs;

    let path = uri.path().trim_start_matches('/');
    let candidate = format!("{}/{path}", options.site_root);
    if let Ok(bytes) = fs::read(&candidate).await {
        let mime = mime_guess::from_path(&candidate).first_or_octet_stream();
        return ([(http::header::CONTENT_TYPE, mime.as_ref())], bytes).into_response();
    }

    // Leptos 0.7: `render_app_to_stream` takes only the app fn; the options
    // are propagated via Router state. We discard `options` (read but unused
    // beyond pulling `site_root`).
    let _ = options;
    let handler = leptos_axum::render_app_to_stream(App);
    handler(req).await.into_response()
}

/// Build the `/mcp` sub-router. Reads `HOLOFS_MCP_TOKEN` from the env
/// once at startup:
///
/// - **unset**: mount the rmcp tower service as-is. Writes are refused
///   inside `holofs-mcp` itself, so the endpoint is read-only and the
///   network exposure is bounded.
/// - **set**: mount the same service behind an `Authorization: Bearer
///   <token>` middleware, and flip the `writes_enabled` flag so the
///   write tools actually run. A request without the right token returns
///   401 before reaching rmcp.
///
/// The token is read once; rotating it requires a restart. Logging only
/// records *whether* a token was configured, never the token itself.
fn build_mcp_router(gateway: Arc<holofs_gateway::Gateway>) -> Router<LeptosOptions> {
    use axum::http::{header, StatusCode};
    use axum::middleware::{from_fn, Next};

    let token = std::env::var("HOLOFS_MCP_TOKEN").ok().filter(|s| !s.is_empty());
    let writes = token.is_some();
    let svc = holofs_mcp::make_mcp_service(gateway, writes);

    info!(
        mcp_writes_enabled = writes,
        "MCP endpoint mounted at /mcp ({})",
        if writes {
            "bearer-auth required"
        } else {
            "read-only, set HOLOFS_MCP_TOKEN to enable writes"
        }
    );

    let mcp_router: Router<LeptosOptions> = Router::new().nest_service("/mcp", svc);

    match token {
        Some(tok) => {
            let expected = format!("Bearer {tok}");
            mcp_router.layer(from_fn(move |req: Request<Body>, next: Next| {
                let expected = expected.clone();
                async move {
                    let got = req
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if got == expected {
                        next.run(req).await
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }
            }))
        }
        None => mcp_router,
    }
}
