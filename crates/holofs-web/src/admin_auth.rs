//! N6 — admin bearer-token authentication.
//!
//! Pre-N6, `/admin/node` and `/api/gc` were open to anyone who could
//! reach the gateway. On a shared network that's a
//! chuckle-inducing footgun: a curious neighbour on the same
//! Kubernetes cluster could `POST /admin/node?idx=17` and knock a
//! node out, or `POST /api/gc` and force a full-cluster shard
//! walk. Neither endpoint appears in the UI's normal navigation,
//! so an operator wouldn't notice the abuse in logs.
//!
//! This module ships one middleware — [`require_admin_token`] —
//! that plugs into a route via
//! `.route_layer(from_fn(move |req, next| { require_admin_token(cfg, req, next) }))`.
//!
//! Behaviour matrix, keyed off two env vars read once at bootstrap:
//!
//! | `HOLOFS_ADMIN_TOKEN` | `HOLOFS_ADMIN_UNAUTHENTICATED` | Header check | Response on miss |
//! |----------------------|-------------------------------|--------------|------------------|
//! | set                  | any                           | required     | 401 Unauthorized |
//! | unset                | `"1"`                         | skipped      | (dev override — every request allowed, WARN at boot) |
//! | unset                | unset                         | skipped      | 403 Forbidden — endpoints are DISABLED, not "open" |
//!
//! The default (both unset) is safe-by-default: rather than
//! silently exposing the admin surface, we refuse the request
//! and force an explicit opt-in. Dev setups that want the
//! pre-N6 behaviour set `HOLOFS_ADMIN_UNAUTHENTICATED=1` and
//! accept the boot-time warning.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Runtime configuration for the admin middleware. Built once at
/// bootstrap from env vars and cloned into every `from_fn` closure.
#[derive(Clone)]
pub struct AdminAuth {
    /// Expected `Authorization: Bearer <token>` value. `None` when
    /// no token is configured.
    pub expected: Option<String>,
    /// Explicit dev override to allow admin routes without a token.
    /// If false and `expected` is None, every admin request returns
    /// 403.
    pub unauthenticated_allowed: bool,
    pub missing_counter: Arc<AtomicU64>,
    pub bad_counter: Arc<AtomicU64>,
    pub disabled_counter: Arc<AtomicU64>,
}

impl AdminAuth {
    /// Read `HOLOFS_ADMIN_TOKEN` + `HOLOFS_ADMIN_UNAUTHENTICATED`
    /// from the process env once. Log the outcome so an operator
    /// starting the daemon can immediately see whether the admin
    /// surface is protected.
    pub fn from_env(
        missing_counter: Arc<AtomicU64>,
        bad_counter: Arc<AtomicU64>,
        disabled_counter: Arc<AtomicU64>,
    ) -> Self {
        let expected = std::env::var("HOLOFS_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|t| format!("Bearer {t}"));
        let unauthenticated_allowed = std::env::var("HOLOFS_ADMIN_UNAUTHENTICATED")
            .ok()
            .as_deref()
            == Some("1");
        match (&expected, unauthenticated_allowed) {
            (Some(_), _) => tracing::info!("admin surface: bearer-token auth enabled"),
            (None, true) => tracing::warn!(
                "admin surface: HOLOFS_ADMIN_TOKEN unset AND \
                 HOLOFS_ADMIN_UNAUTHENTICATED=1 — every /admin/* and \
                 /api/gc request is allowed. Dev-only; DO NOT run \
                 this on a shared network."
            ),
            (None, false) => tracing::warn!(
                "admin surface: DISABLED — set HOLOFS_ADMIN_TOKEN=<token> \
                 to enable, or HOLOFS_ADMIN_UNAUTHENTICATED=1 for a dev \
                 override that leaves the endpoints open. All admin \
                 requests will return 403."
            ),
        }
        Self {
            expected,
            unauthenticated_allowed,
            missing_counter,
            bad_counter,
            disabled_counter,
        }
    }
}

/// Middleware fn plugged into a route via
/// `from_fn(move |req, next| require_admin_token(cfg.clone(), req, next))`.
/// Constant-time byte-slice equality. Compares the whole range so the
/// timing signal does not reveal the position of the first mismatched
/// byte. Length is checked first — that leaks the *expected* token
/// length, which is publicly known (a config value), so 0 bits of
/// secret material.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

pub async fn require_admin_token(
    cfg: AdminAuth,
    req: Request<Body>,
    next: Next,
) -> Response {
    match cfg.expected.as_deref() {
        Some(expected) => {
            let got = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if got.is_empty() {
                cfg.missing_counter.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("admin auth: missing Authorization header");
                return (
                    StatusCode::UNAUTHORIZED,
                    "admin auth required (missing Authorization header)",
                )
                    .into_response();
            }
            if ct_eq(got.as_bytes(), expected.as_bytes()) {
                next.run(req).await
            } else {
                cfg.bad_counter.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("admin auth: bad token");
                (
                    StatusCode::UNAUTHORIZED,
                    "admin auth failed (bad or expired token)",
                )
                    .into_response()
            }
        }
        None if cfg.unauthenticated_allowed => next.run(req).await,
        None => {
            cfg.disabled_counter.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::FORBIDDEN,
                "admin surface disabled — set HOLOFS_ADMIN_TOKEN to enable",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, MethodRouter};
    use tower::Service;

    async fn body_str(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn cfg(expected: Option<&str>, unauth: bool) -> AdminAuth {
        AdminAuth {
            expected: expected.map(|t| format!("Bearer {t}")),
            unauthenticated_allowed: unauth,
            missing_counter: Arc::new(AtomicU64::new(0)),
            bad_counter: Arc::new(AtomicU64::new(0)),
            disabled_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn drive(cfg: AdminAuth, header: Option<&str>) -> (StatusCode, String) {
        let cfg_c = cfg.clone();
        let app: MethodRouter = get(|| async { "ok" }).layer(
            axum::middleware::from_fn(move |req, next| {
                require_admin_token(cfg_c.clone(), req, next)
            }),
        );
        let mut svc = app.with_state(());
        let mut builder = Request::builder().uri("/");
        if let Some(h) = header {
            builder = builder.header(header::AUTHORIZATION, h);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = svc.call(req).await.unwrap();
        let status = resp.status();
        let body = body_str(resp).await;
        (status, body)
    }

    #[tokio::test]
    async fn token_set_correct_bearer_passes() {
        let c = cfg(Some("secret"), false);
        let (status, body) = drive(c, Some("Bearer secret")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn token_set_missing_header_401() {
        let c = cfg(Some("secret"), false);
        let missing = c.missing_counter.clone();
        let (status, _) = drive(c, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(missing.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn token_set_wrong_bearer_401() {
        let c = cfg(Some("secret"), false);
        let bad = c.bad_counter.clone();
        let (status, _) = drive(c, Some("Bearer nope")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(bad.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn no_token_no_override_403() {
        let c = cfg(None, false);
        let disabled = c.disabled_counter.clone();
        let (status, body) = drive(c, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("disabled"), "body was {body:?}");
        assert_eq!(disabled.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn no_token_dev_override_allows() {
        let c = cfg(None, true);
        let (status, body) = drive(c, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }
}
