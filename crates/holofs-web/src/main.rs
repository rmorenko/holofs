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
use axum::middleware::from_fn;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Router};
use clap::Parser;
use leptos::prelude::*;
use leptos_axum::{generate_route_list, handle_server_fns_with_context, LeptosRoutes};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
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
use holofs_web::admin_auth::{require_admin_token, AdminAuth};
use holofs_web::backpressure::with_permit;
use holofs_web::rate_limit::{run_with_rate_limit, RateLimit};
use holofs_web::similar::GetSimilar;
use holofs_web::timeout::{run_with_deadline, LONG, MEDIUM, SHORT};
use holofs_web::{App, GetCatalog, ListDir, ListDirPageFn, Shell};

/// Per-request body cap for upload routes (PUT and the two `/escrow/*`
/// multipart endpoints). axum's default is 2 MiB which rejects realistic
/// media uploads with a misleading "multipart parsing" error. 256 MiB
/// covers typical PNG / WAV / video chunks; the ceiling protects against
/// runaway requests on a shared deployment.
const UPLOAD_BODY_LIMIT: usize = 256 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // TOML config file. Runs before clap so the config's
    // values populate HOLOFS_* env vars, which then feed into
    // clap's normal env-fallback resolution. See
    // `holofs_web::config_file` for the priority ladder + safety
    // note about std::env::set_var being pre-runtime.
    let config_source = if let Some(path) = holofs_web::config_file::detect_config_path() {
        match holofs_web::config_file::ConfigFile::load(&path) {
            Ok(Some(cfg)) => {
                cfg.apply_to_env();
                Some(path)
            }
            Ok(None) => {
                eprintln!(
                    "warning: --config {} does not exist; continuing with env + defaults",
                    path.display()
                );
                None
            }
            Err(e) => {
                eprintln!("error: reading {} failed: {e}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let cli = Cli::parse();
    init_tracing(&cli);
    if let Some(path) = &config_source {
        info!(
            config = %path.display(),
            "loaded TOML config (values shadowed by env + CLI as normal)"
        );
    }

    info!(
        version = env!("CARGO_PKG_VERSION"),
        addr = %cli.addr,
        storage = %cli.storage.display(),
        log_format = ?cli.log_format,
        "starting holofs-web"
    );

    let Bootstrap {
        gateway,
        monitor,
        auditor,
        scrub,
        reputation_persist,
        node_tasks,
        shutdown,
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
    // in-app help / docs viewer.
    server_fn::axum::register_explicit::<GetDoc>();
    server_fn::axum::register_explicit::<ListDocs>();

    let routes = generate_route_list(App);

    // Two clones — one for the leptos route renderer, one for the server-fn
    // handler. Both need the same `Arc<Gateway>` in context.
    let gw_for_routes = Arc::clone(&gateway);
    let gw_for_server_fns = Arc::clone(&gateway);
    // fix: provide `LeptosOptions` as context so the
    // `App` shell can pick it up and render `<HydrationScripts/>`. The
    // bundle (`pkg/holofs.js` + wasm) is never loaded otherwise — the
    // page works as pure SSR with no client-side reactivity, which is
    // exactly the symptom the lazy-tree refactor uncovered.
    let opts_for_routes = leptos_options.clone();

    // N7: HTTP handler timeouts. Routes are grouped into three
    // buckets so a slow / hung cluster can't chain-stall the whole
    // gateway. `timeout::run_with_deadline` returns 504 on elapsed.
    //
    // SHORT (10 s) — read-only introspection: catalog / metrics.
    // A slow response here signals real degradation (locks, cluster
    // stalls). /admin/node moves into its own bucket below (N6).
    let (to_short, to_medium, to_long) = gateway.timeout_counters();
    let to_short_public = to_short.clone();
    let short_routes: Router<LeptosOptions> = Router::new()
        .route("/api/stats", get(handlers::api_stats))
        .route("/metrics", get(handlers::metrics))
        .route_layer(from_fn(move |req, next| {
            run_with_deadline(SHORT, to_short_public.clone(), req, next)
        }));

    // N6: admin surface — auth-gated. Both /admin/node and /api/gc
    // are potentially destructive and pre-N6 were open to anyone
    // who could reach the gateway. `AdminAuth::from_env` reads
    // HOLOFS_ADMIN_TOKEN once at startup and logs the outcome.
    // /admin/node keeps its SHORT (10s) deadline; /api/gc keeps
    // the LONG (5 min) deadline. Neither wears the LONG semaphore
    // — admin calls should just run when the operator asks.
    let (admin_missing, admin_bad, admin_disabled) = gateway.admin_auth_counters();
    let admin_cfg = AdminAuth::from_env(admin_missing, admin_bad, admin_disabled);
    let admin_cfg_short = admin_cfg.clone();
    let admin_short_routes: Router<LeptosOptions> = Router::new()
        .route("/admin/node", post(handlers::toggle_node))
        .route_layer(from_fn(move |req, next| {
            run_with_deadline(SHORT, to_short.clone(), req, next)
        }))
        .route_layer(from_fn(move |req, next| {
            require_admin_token(admin_cfg_short.clone(), req, next)
        }));
    let admin_cfg_long = admin_cfg;
    let to_long_admin = to_long.clone();
    let admin_long_routes: Router<LeptosOptions> = Router::new()
        .route("/api/gc", post(handlers::gc_orphans))
        .route_layer(from_fn(move |req, next| {
            run_with_deadline(LONG, to_long_admin.clone(), req, next)
        }))
        .route_layer(from_fn(move |req, next| {
            require_admin_token(admin_cfg_long.clone(), req, next)
        }));

    // LONG (5 min, LONG-bucket backpressure) — catalog-wide scans
    // and Monte Carlo. These legitimately take minutes on large
    // catalogs; a shorter budget would 504 healthy calls. Bucket
    // permits (default 8) throttle concurrent expensive calls so a
    // burst doesn't saturate the shard cache.
    let (long_sem, long_rej) = gateway.long_bucket();
    let rate_limit = RateLimit::from_env(gateway.rate_limit_rejected_counter());
    let rate_limit_long = rate_limit.clone();
    let long_routes: Router<LeptosOptions> = Router::new()
        .route("/api/search", get(handlers::semantic_search))
        .route("/api/spotlight.png", get(handlers::spotlight_png))
        .route("/api/embed_all", post(handlers::embed_all))
        .route("/api/fingerprint/*path", get(handlers::api_fingerprint))
        .route_layer(from_fn(move |req, next| {
            run_with_deadline(LONG, to_long.clone(), req, next)
        }))
        .route_layer(from_fn(move |req, next| {
            with_permit(long_sem.clone(), long_rej.clone(), req, next)
        }))
        // per-IP rate limit applied ABOVE backpressure so a
        // rejected client doesn't consume a MEDIUM/LONG permit.
        // Layer is a no-op when HOLOFS_RATE_LIMIT_RPS_PER_IP=0
        // (default).
        .route_layer(from_fn(move |req, next| {
            run_with_rate_limit(rate_limit_long.clone(), req, next)
        }));

    // STREAMING — SSE + multipart/x-mixed-replace. Intentionally
    // unbudgeted: the timer would start on the first byte and kill
    // the stream at the deadline.
    let streaming_routes: Router<LeptosOptions> = Router::new()
        .route("/api/health/events", get(handlers::health_events))
        // streaming hologram — multipart/x-mixed-replace
        // body re-rendered for every layer from L0 to full.
        .route("/preview/stream/*name", get(handlers::preview_stream));

    // MEDIUM (60 s) — everything else: server_fns, decode / PUT /
    // DELETE, directory ops, mix, diff, inspect, escrow, static
    // assets under /pkg + /assets. Generous ceiling so a healthy
    // but under-load cluster doesn't 504 spuriously.
    let medium_routes: Router<LeptosOptions> = Router::new()
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
        // directory operations. Two flavours per op — the
        // path-wildcard JSON variants for API/curl users, plus a
        // form-urlencoded POST that the catalog page's HTML forms can hit
        // without JavaScript. The form variants 303-redirect on success.
        .route("/api/mkdir/*path", post(handlers::mkdir))
        .route("/api/mkdir", post(handlers::mkdir_form))
        .route("/api/rmdir/*path", delete(handlers::rmdir))
        .route("/api/rmdir", post(handlers::rmdir_form))
        // form-friendly file delete (mirror of rmdir_form
        // for non-directory entries).
        .route("/api/rm", post(handlers::rm_form))
        // wavelet mix UI plumbing.
        .route("/api/mix.png", get(handlers::mix_preview))
        .route("/api/mix-save", post(handlers::mix_save))
        // form-friendly version restore.
        .route("/api/restore", post(handlers::restore_version_form))
        // .x: form-friendly version deletion (per-row "delete"
        // button on /versions/<name>). Removes the .bin archive and
        // GCs any shards it uniquely held.
        .route("/api/versions/delete", post(handlers::delete_version_form))
        .route("/api/mv", post(handlers::mv))
        // form-friendly file upload from the catalog page.
        .route(
            "/api/upload",
            post(handlers::upload_form).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        // bypass axum's 2 MiB default body limit for routes
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
        // c_l_idx leads so the wildcard can capture multi-segment
        // object paths after it.
        .route(
            "/api/shard/:c_l_idx/*path",
            get(handlers::get_shard_png),
        )
        .route("/preview/*path", get(handlers::get_preview))
        .route("/*path", get(handlers::get_object))
        // streaming PUT: no DefaultBodyLimit — the handler
        // streams the body to a tempfile under `<storage>/uploads/`
        // and enforces `HOLOFS_UPLOAD_MAX_SIZE` (default 1 GiB) per
        // request. `DefaultBodyLimit::disable()` overrides the
        // 2 MiB axum default so the streaming path sees the full
        // body.
        .route(
            "/*path",
            put(handlers::put_object).layer(DefaultBodyLimit::disable()),
        )
        .route("/*path", delete(handlers::delete_object))
        .route_layer(from_fn(move |req, next| {
            run_with_deadline(MEDIUM, to_medium.clone(), req, next)
        }));
    // N3: MEDIUM-bucket backpressure. Cap default 64 concurrent
    // decodes / PUT / dir-ops so a burst can't DoS the process.
    // per-IP rate limit stacks on top so a single client
    // can't exhaust MEDIUM permits and freeze out the rest.
    let (medium_sem, medium_rej) = gateway.medium_bucket();
    let rate_limit_medium = rate_limit.clone();
    let medium_routes = medium_routes
        .route_layer(from_fn(move |req, next| {
            with_permit(medium_sem.clone(), medium_rej.clone(), req, next)
        }))
        .route_layer(from_fn(move |req, next| {
            run_with_rate_limit(rate_limit_medium.clone(), req, next)
        }));

    let app = Router::new()
        .merge(short_routes)
        .merge(admin_short_routes)
        .merge(admin_long_routes)
        .merge(long_routes)
        .merge(streaming_routes)
        .merge(medium_routes)
        // + 12.1: MCP (Model Context Protocol) server over
        // Streamable HTTP. The tower service handles POST/GET/DELETE on
        // `/mcp` per the spec — wire it as `nest_service` so axum hands
        // the whole sub-path off to rmcp instead of routing per-method.
        // Tools share the cluster's live `Arc<Gateway>`, so MCP clients
        // see the same catalog as the UI. Intentionally UNBUDGETED
        // (MCP Streamable HTTP holds the connection open).
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
                provide_context(opts_for_routes.clone());
            },
            {
                let opts = leptos_options.clone();
                move || view! { <Shell options=opts.clone()/> }
            },
        )
        // serve the leptos wasm/js bundle.
        // - `Cache-Control: no-cache` forces a 304/200 revalidation
        //   on every page load so a `cargo leptos build` rebuild
        //   isn't shadowed by a stale browser-cached copy.
        // - The dedicated `/pkg/holofs_bg.wasm` route papers over a
        //   cargo-leptos 0.3.6 ↔ wasm-bindgen filename mismatch:
        //   wasm-bindgen's JS glue hardcodes `import('holofs_bg.wasm')`
        //   but cargo-leptos saves the binary as `holofs.wasm`. We
        //   serve the same bytes under either name so the browser
        //   stops 404-ing the wasm fetch and hydrate actually runs.
        .route(
            "/pkg/holofs_bg.wasm",
            get(handlers::serve_wasm_alias),
        )
        .nest_service(
            "/pkg",
            tower::ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    http::header::CACHE_CONTROL,
                    http::HeaderValue::from_static("no-cache"),
                ))
                .service(ServeDir::new(format!("{site_root}/pkg"))),
        )
        // static assets used by the upload form, the help
        // viewer (Mermaid + KaTeX bootstrap), and anything else dropped
        // into `crates/holofs-web/assets/`.
        //
        // `ServeDir` falls back to the source directory if
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

    // N1: cancel the shared shutdown token on SIGTERM / SIGINT so axum
    // + every background loop start winding down together.
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("shutdown signal received, draining");
        signal_shutdown.cancel();
    });

    // `with_graceful_shutdown` stops accepting new connections when the
    // token fires. axum then waits (until this future's cancellation
    // future resolves; we hand it the same token) for in-flight
    // requests to complete before returning from `.await`.
    let axum_shutdown = shutdown.clone();
    // `with_connect_info` injects the peer SocketAddr into
    // every request's extensions so `rate_limit::client_ip` can
    // extract it. Zero cost when the rate limit is disabled — the
    // middleware short-circuits on `enabled() == false`.
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        axum_shutdown.cancelled().await;
    });
    if let Err(e) = serve.await {
        tracing::error!(error = %e, "axum::serve returned error");
    }
    info!("axum serve loop drained; joining background tasks");

    // N1: give background loops a chance to finish their current tick
    // then abort node listeners so the ports free up promptly. In-
    // flight per-connection tasks are independent of the parent
    // listener and finish on their own.
    let drain = std::time::Duration::from_secs(10);
    let joined = tokio::time::timeout(drain, async {
        let _ = monitor.await;
        let _ = auditor.await;
        if let Some(s) = scrub {
            let _ = s.await;
        }
        // N5: wait for the reputation-persist task to write its
        // final snapshot before we abort node listeners. Otherwise
        // the last batch of auditor observations gets lost.
        let _ = reputation_persist.await;
    })
    .await;
    if joined.is_err() {
        tracing::warn!(
            timeout_secs = drain.as_secs(),
            "background tasks did not drain within timeout; forcing abort"
        );
    }
    for t in node_tasks {
        t.abort();
    }
    info!("holofs-web shutdown complete");
}

/// N1: block until the process receives SIGTERM or SIGINT.
///
/// On Unix we listen for both signals; on Windows we fall back to
/// `ctrl_c` which is the only portable equivalent. Either arm resolves
/// the whole future — we only need the first signal to fire.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "install SIGTERM handler failed");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "install SIGINT handler failed");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM"),
            _ = sigint.recv() => info!("SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "ctrl_c handler failed");
        } else {
            info!("Ctrl-C");
        }
    }
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

    // For the fallback path we need to provide `LeptosOptions` into
    // the render context and render the same `Shell` wrapper used on
    // routed pages so `<HydrationScripts/>` ends up in `<head>`.
    let opts = options;
    let opts_for_ctx = opts.clone();
    let handler = leptos_axum::render_app_to_stream_with_context(
        move || provide_context(opts_for_ctx.clone()),
        move || view! { <Shell options=opts.clone()/> },
    );
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
