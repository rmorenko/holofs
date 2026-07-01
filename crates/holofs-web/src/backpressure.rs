//! N3 — bounded-concurrency backpressure for HTTP routes.
//!
//! Pre-N3, holofs-web spawned a fresh axum task for every incoming
//! request with no ceiling. Under a burst — say a client hammering
//! `/api/upload` in parallel — those tasks all queued on the same
//! 40-node cluster's `put_object` semaphore and the process happily
//! chewed through memory + file descriptors until either the OS or
//! the reverse proxy killed something.
//!
//! [`with_permit`] wraps a route in a `try_acquire_owned` against
//! the passed [`Semaphore`]. If a permit is available the request
//! proceeds and holds the permit until `next.run(req)` resolves
//! (`OwnedSemaphorePermit` auto-releases on drop). If no permit is
//! free the middleware bumps `rejected` and returns 503 Service
//! Unavailable so a well-behaved client backs off — instead of
//! piling axum tasks on the runtime.
//!
//! Two buckets, each configured via env at bootstrap:
//! - **MEDIUM** — decodes / PUT / directory ops. Default cap 64
//!   (`HOLOFS_MEDIUM_CONCURRENCY`).
//! - **LONG** — semantic search / spotlight / GC. Default cap 8
//!   (`HOLOFS_LONG_CONCURRENCY`).
//!
//! SHORT bucket (catalog reads, /metrics) and streaming endpoints
//! (SSE, multipart) are intentionally *not* rate-limited. They are
//! cheap enough or long-lived enough that a permit ceiling would
//! either be pointless or actively harmful.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;

/// Acquire one permit on `sem` for the duration of the request, or
/// return `503 Service Unavailable` immediately if the semaphore is
/// at capacity. Rejection is counted via `rejected` for `/metrics`.
///
/// Callers wire this into a route group via:
/// ```ignore
/// let (sem, rej) = gateway.medium_bucket();
/// router.route_layer(from_fn(move |req, next| {
///     with_permit(sem.clone(), rej.clone(), req, next)
/// }))
/// ```
pub async fn with_permit(
    sem: Arc<Semaphore>,
    rejected: Arc<AtomicU64>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // `try_acquire_owned` is cheap: a single CAS on the semaphore
    // counter, no async wait. That's the point — we WANT to reject
    // instantly under load rather than growing the axum task queue.
    match Arc::clone(&sem).try_acquire_owned() {
        Ok(_permit) => next.run(req).await,
        Err(_) => {
            rejected.fetch_add(1, Ordering::Relaxed);
            let cap = sem.available_permits();
            tracing::warn!(
                available = cap,
                total_rejected = rejected.load(Ordering::Relaxed),
                "backpressure rejected request (503)"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "server at capacity ({cap} permits available), retry with backoff"
                ),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, MethodRouter};

    /// Drive `with_permit` against a synthetic router where the
    /// handler just returns 200. `pre_held` permits are acquired
    /// (and forgotten) before the call so the semaphore starts at
    /// `cap - pre_held` available.
    async fn call_once(cap: usize, pre_held: usize) -> (StatusCode, u64) {
        let sem = Arc::new(Semaphore::new(cap));
        let rejected = Arc::new(AtomicU64::new(0));
        // Forget `pre_held` permits so the semaphore is artificially
        // occupied for the duration of the test. `forget` drops the
        // permit without releasing — same effect as an in-flight
        // request holding it.
        for _ in 0..pre_held {
            sem.clone().try_acquire_owned().unwrap().forget();
        }

        let sem_c = sem.clone();
        let rej_c = rejected.clone();
        let app: MethodRouter = get(|| async { "ok" }).layer(
            axum::middleware::from_fn(move |req, next| {
                with_permit(sem_c.clone(), rej_c.clone(), req, next)
            }),
        );

        use tower::Service;
        let mut svc = app.with_state(());
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = svc.call(req).await.expect("service call");
        let status = resp.status();
        let _ = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        (status, rejected.load(Ordering::Relaxed))
    }

    #[tokio::test]
    async fn permit_available_returns_200() {
        let (status, rej) = call_once(4, 0).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rej, 0, "no rejection counter bump");
    }

    #[tokio::test]
    async fn no_permit_returns_503_and_increments_counter() {
        // cap=2, all pre-held → semaphore has 0 free → 503.
        let (status, rej) = call_once(2, 2).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rej, 1, "rejection counter should have incremented once");
    }
}
