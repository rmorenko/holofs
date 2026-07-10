//! per-client-IP rate limit middleware.
//!
//! N3 backpressure () is a **global** cap per route bucket:
//! MEDIUM = 64 concurrent decodes, LONG = 8 concurrent scans, etc.
//! Under that ceiling a single misbehaving client can still starve
//! every other caller — it will keep grabbing permits as they free
//! up, indefinitely.
//!
//! This module adds a **per-IP** ceiling on top: each unique client
//! address gets its own token bucket refilled at
//! `HOLOFS_RATE_LIMIT_RPS_PER_IP` requests per second, capped at
//! `HOLOFS_RATE_LIMIT_BURST` tokens. When a request arrives and the
//! bucket is empty the middleware returns `429 Too Many Requests`
//! with a `Retry-After: 1` header. The N3 semaphore is *below* this
//! layer, so even a compliant client can't blow past the global
//! backpressure caps.
//!
//! Both knobs default to 0 (disabled) so pre-clusters and
//! private deployments don't get surprise 429s. Set both to enable
//! (typical prod tune for a small dev cluster:
//! `rps_per_ip = 20, burst = 40`).
//!
//! Client-IP source: `ConnectInfo<SocketAddr>` from
//! `into_make_service_with_connect_info` in main.rs, or the first
//! comma-separated token of `X-Forwarded-For` when the request came
//! through a reverse proxy. Falls back to a shared "unknown"
//! bucket when neither is present — noisy hosts don't get a
//! per-conn free pass.
//!
//! State is `Arc<Mutex<HashMap<IpAddr, TokenBucket>>>` with a
//! monotonic sweep that evicts entries idle longer than
//! `HOLOFS_RATE_LIMIT_IDLE_SECS` (default 300 s) on each check —
//! keeps memory bounded under high-churn client populations.

#![cfg(feature = "ssr")]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;

/// Runtime configuration + shared per-IP token-bucket table.
/// Clone-cheap (only Arcs inside) so `from_fn` closures can capture
/// it by move for each middleware layer.
#[derive(Clone)]
pub struct RateLimit {
    /// Refill rate. `None` = disabled.
    rps_per_ip: Option<f64>,
    /// Max tokens a bucket holds. `None` = disabled.
    burst: Option<f64>,
    /// Idle-eviction threshold for the sweep.
    idle: Duration,
    /// Per-IP state. Also holds the "unknown" bucket keyed at
    /// `0.0.0.0`.
    buckets: Arc<Mutex<HashMap<IpAddr, TokenBucket>>>,
    /// Cumulative 429 responses (public counter, wired into
    /// `/metrics` — see main.rs).
    pub rejected_total: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

impl RateLimit {
    /// Build a config from env, wiring the given rejection counter
    /// so `/metrics` and the middleware share the same atomic.
    /// When either `rps_per_ip` or `burst` resolves to 0 (the
    /// default) the whole layer is a no-op — every request passes
    /// through untouched.
    pub fn from_env(rejected_total: Arc<AtomicU64>) -> Self {
        let sec = &crate::runtime_config::RuntimeConfig::get().security;
        let rps = sec.rate_limit_rps_per_ip;
        let burst = if sec.rate_limit_burst > 0.0 {
            sec.rate_limit_burst
        } else {
            rps * 2.0
        };
        let idle_secs = sec.rate_limit_idle_secs;
        let enabled = rps > 0.0 && burst > 0.0;
        if enabled {
            tracing::info!(
                rps_per_ip = rps,
                burst,
                idle_secs,
                "per-IP rate limit enabled"
            );
        } else {
            tracing::info!(
                "per-IP rate limit disabled (set HOLOFS_RATE_LIMIT_RPS_PER_IP>0 to enable)"
            );
        }
        Self {
            rps_per_ip: enabled.then_some(rps),
            burst: enabled.then_some(burst),
            idle: Duration::from_secs(idle_secs),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            rejected_total,
        }
    }

    /// Whether the layer is a no-op. Callers can short-circuit
    /// bookkeeping when it is.
    pub fn enabled(&self) -> bool {
        self.rps_per_ip.is_some() && self.burst.is_some()
    }

    /// Try to spend one token for `ip`. Returns `true` if the
    /// request should proceed. Refills the bucket lazily on each
    /// call and evicts stale entries when the map traversal
    /// notices them.
    async fn try_take(&self, ip: IpAddr) -> bool {
        let (Some(rps), Some(burst)) = (self.rps_per_ip, self.burst) else {
            return true;
        };
        let now = Instant::now();
        let mut map = self.buckets.lock().await;

        // Cheap eviction: on each call inspect at most 4 random
        // buckets (via `iter().take`) and drop the ones idle beyond
        // the threshold. Full sweeps are avoided so the O(N) cost
        // never lands in a single request's critical path.
        let idle = self.idle;
        let stale: Vec<IpAddr> = map
            .iter()
            .take(4)
            .filter_map(|(k, b)| {
                if now.duration_since(b.last_seen) > idle {
                    Some(*k)
                } else {
                    None
                }
            })
            .collect();
        for k in stale {
            map.remove(&k);
        }

        let entry = map.entry(ip).or_insert_with(|| TokenBucket {
            tokens: burst,
            last_refill: now,
            last_seen: now,
        });
        // Refill.
        let elapsed = now.duration_since(entry.last_refill).as_secs_f64();
        entry.tokens = (entry.tokens + elapsed * rps).min(burst);
        entry.last_refill = now;
        entry.last_seen = now;
        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Extract the client IP from the request. Tries in order:
///
/// 1. First hop in `X-Forwarded-For` (behind a reverse proxy).
/// 2. `ConnectInfo<SocketAddr>` from
///    `into_make_service_with_connect_info` (direct connection).
/// 3. Fallback `0.0.0.0` (shared "unknown" bucket).
fn client_ip(req: &Request<Body>) -> IpAddr {
    if let Some(xff) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(first) = xff.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }
    if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<std::net::SocketAddr>>() {
        return addr.ip();
    }
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// Middleware body. Uses `req.extensions()` for `ConnectInfo` so we
/// don't need a separate extractor.
pub async fn run_with_rate_limit(
    rl: RateLimit,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !rl.enabled() {
        return next.run(req).await;
    }
    let ip = client_ip(&req);
    if rl.try_take(ip).await {
        next.run(req).await
    } else {
        rl.rejected_total.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            %ip,
            total_rejected = rl.rejected_total.load(Ordering::Relaxed),
            "rate limit rejected request (429)"
        );
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::RETRY_AFTER, "1"),
                (header::CONTENT_TYPE, "text/plain"),
            ],
            format!("rate limit hit for client {ip}; retry after 1s"),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn cfg(rps: f64, burst: f64) -> RateLimit {
        RateLimit {
            rps_per_ip: Some(rps),
            burst: Some(burst),
            idle: Duration::from_secs(60),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            rejected_total: Arc::new(AtomicU64::new(0)),
        }
    }

    #[tokio::test]
    async fn burst_allows_up_to_capacity() {
        let rl = cfg(1.0, 3.0);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        // Burst 3 → three allowed, fourth rejected.
        assert!(rl.try_take(ip).await);
        assert!(rl.try_take(ip).await);
        assert!(rl.try_take(ip).await);
        assert!(!rl.try_take(ip).await);
    }

    #[tokio::test]
    async fn refill_replenishes_tokens() {
        let rl = cfg(1000.0, 1.0); // very high rate for quick refill
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        assert!(rl.try_take(ip).await);
        assert!(!rl.try_take(ip).await);
        tokio::time::sleep(Duration::from_millis(5)).await;
        // 5ms × 1000 rps = 5 tokens → next call should succeed.
        assert!(rl.try_take(ip).await);
    }

    #[tokio::test]
    async fn separate_ips_have_separate_buckets() {
        let rl = cfg(1.0, 1.0);
        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 10));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11));
        assert!(rl.try_take(ip1).await);
        assert!(!rl.try_take(ip1).await);
        // ip2 has its own bucket, still full.
        assert!(rl.try_take(ip2).await);
    }

    #[tokio::test]
    async fn disabled_config_passes_everything() {
        let rl = RateLimit {
            rps_per_ip: None,
            burst: None,
            idle: Duration::from_secs(60),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            rejected_total: Arc::new(AtomicU64::new(0)),
        };
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 20));
        for _ in 0..1000 {
            assert!(rl.try_take(ip).await);
        }
    }
}
