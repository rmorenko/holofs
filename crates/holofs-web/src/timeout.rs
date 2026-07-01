//! N7 — per-route HTTP handler timeouts.
//!
//! A slow (or hung) cluster used to pin holofs-web handlers
//! indefinitely: a `GET /photo.png` waiting on a broken decode kept
//! its axum task alive until the client hung up, and there was no
//! backstop above the [`holofs_client`] RPC-level timeout. That's
//! fine for a local dev cluster; it's a footgun in production where
//! a bad node can chain-stall every request into a slow-loris kill.
//!
//! This module ships one primitive — [`run_with_deadline`] — that
//! wraps `next.run(req)` in a `tokio::time::timeout` and turns the
//! elapsed case into a `504 Gateway Timeout` response. Callers
//! plug it into a route via
//! `.route_layer(from_fn(|req, next| run_with_deadline(SHORT, req, next)))`
//! (see main.rs for the three-bucket assignment).
//!
//! The three "buckets" main.rs picks from:
//!
//! * [`SHORT`] (10 s) — read-only introspection endpoints
//!   (`/api/stats`, `/api/list`, `/metrics`). Anything that only
//!   touches the catalog mutex + admin_kills snapshot. A slow
//!   response here signals cluster or lock-contention degradation.
//! * [`MEDIUM`] (60 s) — decode / PUT / directory ops / diff /
//!   inspect / mix / mkdir / delete / restore. The typical
//!   read/write surface. 60 s is deliberately generous so a
//!   healthy but under-load cluster doesn't 504 spuriously.
//! * [`LONG`] (5 min) — endpoints that scan the whole catalog or
//!   run 5 000-trial Monte Carlo: `/api/search`, `/similar/*`,
//!   `/api/spotlight.png`, `/health/*name`, `/api/gc`,
//!   `/api/embed_all`.
//!
//! Streaming endpoints (`/api/health/events` SSE and
//! `/preview/stream/*name` multipart) are intentionally *not*
//! wrapped: the timer starts when the handler begins producing
//! bytes and would kill an SSE stream at the deadline.

use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Short bucket — catalog-only reads (`/api/stats`, `/api/list`, `/metrics`).
pub const SHORT: Duration = Duration::from_secs(10);
/// Medium bucket — decode, PUT, most read/write ops.
pub const MEDIUM: Duration = Duration::from_secs(60);
/// Long bucket — catalog-wide scans and Monte Carlo passes.
pub const LONG: Duration = Duration::from_secs(300);

/// Wrap `next.run(req)` in a `tokio::time::timeout` and return
/// `504 Gateway Timeout` with a short text body on elapsed.
///
/// Usage inside a route builder:
/// ```ignore
/// use axum::middleware::from_fn;
/// use holofs_web::timeout::{run_with_deadline, MEDIUM};
///
/// Router::new()
///     .route("/api/things", get(list_things))
///     .route_layer(from_fn(|req, next| run_with_deadline(MEDIUM, req, next)))
/// ```
pub async fn run_with_deadline(
    dur: Duration,
    req: Request<Body>,
    next: Next,
) -> Response {
    match tokio::time::timeout(dur, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(
                deadline_secs = dur.as_secs(),
                path = %req_path_or_empty(),
                "handler exceeded deadline, returning 504"
            );
            (
                StatusCode::GATEWAY_TIMEOUT,
                format!("handler exceeded {}s deadline", dur.as_secs()),
            )
                .into_response()
        }
    }
}

/// Placeholder — the request URI is already consumed by `next.run` by
/// the time we log. Keep the field in the log record so operators
/// can grep for `path=""` and correlate with the surrounding trace
/// span (the TraceLayer above logs the URI on `on_response` for
/// every request that returns).
#[inline]
fn req_path_or_empty() -> &'static str {
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    async fn body_str(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Build a `Next` around a single handler by materialising a
    /// one-route router as a `Service`. `Next` is intentionally
    /// crate-private in axum, so we get it back only by calling
    /// [`axum::middleware::from_fn`] and peeking through the layer.
    /// It's easier to construct the fake Next by calling
    /// `next.run(req)` for a synthetic router. But axum 0.7 doesn't
    /// expose `Next::from_fn`, so we build the layer + a matched
    /// handler using the public middleware API and let the layer
    /// drive `run_with_deadline` for us.
    async fn drive(dur: Duration, handler_delay: Duration) -> (StatusCode, String) {
        // Emulate the "next" contract by wrapping a plain future
        // that sleeps for `handler_delay` then returns 200. We
        // don't need a Router — just call the pieces
        // `run_with_deadline` cares about.
        use axum::middleware::{from_fn, Next};
        use axum::routing::MethodRouter;

        let app: MethodRouter = get(move || async move {
            tokio::time::sleep(handler_delay).await;
            "ok"
        })
        .layer(from_fn(move |req, next: Next| {
            run_with_deadline(dur, req, next)
        }));

        // Call the MethodRouter directly by turning it into a
        // Service — MethodRouter implements Service<Request>.
        use tower::Service;
        let mut svc = app.with_state(());
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = svc.call(req).await.expect("service call");
        (resp.status(), body_str(resp).await)
    }

    #[tokio::test]
    async fn short_handler_passes_through() {
        let (status, body) = drive(Duration::from_millis(500), Duration::from_millis(10)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn slow_handler_yields_504() {
        let (status, body) = drive(Duration::from_millis(80), Duration::from_secs(2)).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(body.contains("deadline"), "body was {body:?}");
    }
}
