//! Connection pool for client→node wire transport.
//!
//! Each node accepts framed wire requests over a long-lived TCP (or TLS)
//! connection — see `holofs_storage::node_service::handle_connection`: the
//! server reads frames in a loop and replies on the same stream until the
//! peer closes it. So a pool of post-handshake [`TransportStream`]s, keyed
//! by the node's socket-address string, is enough to keep the RPC layer
//! from re-opening (and burning an ephemeral port + a TLS handshake) on
//! every single shard PUT.
//!
//! ## Lifecycle
//!
//! 1. `acquire(addr)` returns the most-recently-released stream for that
//!    addr if one is still fresh; otherwise it dials a new one via
//!    `transport::connect`.
//! 2. The returned [`Pooled`] wraps the stream and implements
//!    `AsyncRead + AsyncWrite + Unpin`, so the wire helpers
//!    (`read_frame`, `write_frame`) work on it unchanged.
//! 3. When [`Pooled`] is dropped, the stream is returned to the pool —
//!    unless [`Pooled::poison`] was called first (e.g., after an IO
//!    error), in which case the stream is closed.
//!
//! ## Env knobs
//!
//! * `HOLOFS_POOL_PER_NODE` — max idle entries per addr (default `8`).
//! * `HOLOFS_POOL_IDLE_SECS` — drop entries older than this on acquire
//!   (default `30`).
//! * `HOLOFS_POOL_DISABLE=1` — bypass the pool entirely; every
//!   `acquire` opens a fresh connection and never returns it.
//!
//! ## Safety / contention
//!
//! Pool state is a single `Mutex<HashMap<…>>` — contended only while
//! popping or pushing an entry, never during the actual RPC. The pool
//! is process-global; tests against a freshly-spawned node get their
//! own slot since the addr changes each run.
//!
//! ## Not solved here
//!
//! Concurrent acquires for the SAME addr while N idle entries < N
//! concurrent callers will each dial a fresh connection. That is the
//! desired behaviour (per-node parallelism), but it does mean the pool
//! upper-bounds idle-only, not in-flight.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::transport::{self, TransportStream};

struct Entry {
    stream: TransportStream,
    last_used: Instant,
}

#[derive(Default)]
struct PoolState {
    per_addr: HashMap<String, VecDeque<Entry>>,
}

fn state() -> &'static Mutex<PoolState> {
    static STATE: OnceLock<Mutex<PoolState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(PoolState::default()))
}

fn per_node_cap() -> usize {
    std::env::var("HOLOFS_POOL_PER_NODE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(8)
}

fn idle_ttl() -> Duration {
    let secs: u64 = std::env::var("HOLOFS_POOL_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

fn disabled() -> bool {
    matches!(std::env::var("HOLOFS_POOL_DISABLE").ok().as_deref(), Some("1") | Some("true"))
}

/// Acquire a connection to `addr`, reusing an idle one if available.
///
/// Returns a [`Pooled`] guard. The underlying stream is returned to the
/// pool when the guard is dropped, unless [`Pooled::poison`] was called.
pub async fn acquire(addr: &str) -> io::Result<Pooled> {
    if disabled() {
        let stream = transport::connect(addr).await?;
        // keep_on_drop=false so a disabled-pool acquire never leaks
        // a TCP connection into the cache after the env knob flips.
        return Ok(Pooled::detached(addr.to_string(), stream));
    }
    if let Some(stream) = pop_fresh(addr) {
        return Ok(Pooled::new(addr.to_string(), stream, true));
    }
    let stream = transport::connect(addr).await?;
    Ok(Pooled::new(addr.to_string(), stream, false))
}

fn pop_fresh(addr: &str) -> Option<TransportStream> {
    let ttl = idle_ttl();
    let now = Instant::now();
    let mut guard = state().lock().ok()?;
    let queue = guard.per_addr.get_mut(addr)?;
    while let Some(entry) = queue.pop_back() {
        if now.duration_since(entry.last_used) <= ttl {
            return Some(entry.stream);
        }
        // expired — drop on the floor (closing the connection)
    }
    None
}

fn release(addr: &str, stream: TransportStream) {
    let cap = per_node_cap();
    let mut guard = match state().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let queue = guard.per_addr.entry(addr.to_string()).or_default();
    if queue.len() >= cap {
        // Drop the oldest entry to make room; LIFO discipline keeps the
        // hot end of the queue, which is what we want to reuse next.
        queue.pop_front();
    }
    queue.push_back(Entry {
        stream,
        last_used: Instant::now(),
    });
}

/// Returned by [`acquire`]. Wraps a [`TransportStream`].
///
/// **Drop semantics** (2026-07-03 rework): the default is to
/// **discard** the underlying stream, NOT recycle it. A caller
/// that has completed a clean, on-frame-boundary exchange must
/// signal that explicitly with [`Pooled::mark_clean`]; only then
/// does Drop return the stream to the pool.
///
/// Rationale: a `Pooled` can be dropped in the middle of a read
/// or write from any number of async cancellation paths —
/// `tokio::time::timeout` on the RPC itself, an outer
/// `tokio::select!` racing another future, a per-handler
/// deadline in the gateway middleware. All of those leave the
/// socket at an undefined byte boundary. Making the default
/// "recycle" turned every such cancellation into pool
/// contamination: the next borrower would read the tail of some
/// other caller's response and surface it as
/// `UnexpectedResponse` (see the 2026-07-03 field bug —
/// "got Shards([])" on a PUT after a monitor-loop cancellation).
///
/// Making the default "discard" flips the safety story: forgetting
/// to `mark_clean()` only costs one extra TCP dial per RPC, never
/// data-corrupts the next borrower.
pub struct Pooled {
    addr: String,
    stream: Option<TransportStream>,
    /// True once the caller confirmed the exchange ended on a
    /// clean frame boundary. Only clean streams recycle to the
    /// pool.
    clean: bool,
    /// True if this stream came out of the pool (i.e. was previously
    /// used). Callers can read this via [`Pooled::was_reused`] to know
    /// whether a retry-on-error is justified.
    reused: bool,
    /// When false, the stream is dropped on `Drop` instead of being
    /// returned to the cache. Set when `HOLOFS_POOL_DISABLE=1` at
    /// acquire time, so toggling the env var mid-flight can't cause
    /// a leak.
    keep_on_drop: bool,
}

impl Pooled {
    fn new(addr: String, stream: TransportStream, reused: bool) -> Self {
        Self {
            addr,
            stream: Some(stream),
            clean: false,
            reused,
            keep_on_drop: true,
        }
    }

    fn detached(addr: String, stream: TransportStream) -> Self {
        Self {
            addr,
            stream: Some(stream),
            clean: false,
            reused: false,
            keep_on_drop: false,
        }
    }

    /// Signal that the RPC exchange finished cleanly on a frame
    /// boundary. Must be called AFTER a successful write + read +
    /// decode round-trip; only then will the underlying stream be
    /// returned to the pool on Drop. Forgetting to call this is
    /// safe — the connection is simply closed and re-dialled on
    /// the next `acquire`.
    pub fn mark_clean(&mut self) {
        self.clean = true;
    }

    /// Retained for API compat — makes it explicit that the
    /// stream is bad. Equivalent to just not calling
    /// [`Self::mark_clean`], but expresses intent at error sites
    /// where the caller wants a self-documenting statement.
    pub fn poison(&mut self) {
        self.clean = false;
    }

    /// Whether this connection was reused from the pool (vs. freshly
    /// dialed). Useful for deciding whether to retry-on-error: a
    /// pooled stream might have been silently closed by the peer
    /// while idle, so retrying once on a new dial is reasonable.
    pub fn was_reused(&self) -> bool {
        self.reused
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if !self.clean || !self.keep_on_drop {
            return;
        }
        if let Some(stream) = self.stream.take() {
            release(&self.addr, stream);
        }
    }
}

impl AsyncRead for Pooled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let stream = self
            .stream
            .as_mut()
            .expect("Pooled stream taken before drop");
        Pin::new(stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Pooled {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = self
            .stream
            .as_mut()
            .expect("Pooled stream taken before drop");
        Pin::new(stream).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = self
            .stream
            .as_mut()
            .expect("Pooled stream taken before drop");
        Pin::new(stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = self
            .stream
            .as_mut()
            .expect("Pooled stream taken before drop");
        Pin::new(stream).poll_shutdown(cx)
    }
}

/// Snapshot of pool occupancy — for tests and `/api/stats`-style
/// introspection. Returns `(addrs_tracked, total_idle_streams)`.
pub fn stats() -> (usize, usize) {
    let guard = match state().lock() {
        Ok(g) => g,
        Err(_) => return (0, 0),
    };
    let addrs = guard.per_addr.len();
    let total = guard.per_addr.values().map(|q| q.len()).sum();
    (addrs, total)
}

/// Drop every cached entry. Used by tests; production code should never
/// need this.
pub fn clear() {
    if let Ok(mut guard) = state().lock() {
        guard.per_addr.clear();
    }
}

/// Shared mutex serialising every test in this crate that touches
/// the global pool cache or the env vars driving it. Pool tests and
/// the RPC-level tests in `client::rpc_tests` both acquire it so
/// they never overlap — without serialisation, a `HOLOFS_POOL_DISABLE=1`
/// set by one test bleeds into a parallel test asserting reuse.
#[cfg(test)]
pub(crate) fn test_pool_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex as StdMutex, OnceLock};
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use holofs_storage::node_service::spawn_node;
    use holofs_wire::{read_frame, write_frame, Request, Response};

    fn pool_test_lock() -> std::sync::MutexGuard<'static, ()> {
        super::test_pool_lock()
    }

    /// Restores a process env var on drop, even if the test panics.
    /// Without this, an assertion failure in one test leaks knobs
    /// like `HOLOFS_POOL_DISABLE=1` into every subsequent test.
    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Helper: send Ping/Pong over a pooled stream. Returns the addr
    /// of the local endpoint so the test can verify reuse across calls.
    async fn ping(addr: &str) -> (Response, std::net::SocketAddr) {
        let mut pooled = acquire(addr).await.expect("acquire");
        let local = match pooled
            .stream
            .as_ref()
            .expect("stream present")
        {
            TransportStream::Plain(t) => t.local_addr().expect("local_addr"),
            TransportStream::Tls(_) => unreachable!("tests use plain transport"),
        };
        write_frame(&mut pooled, &Request::Ping.encode())
            .await
            .expect("write");
        let buf = read_frame(&mut pooled).await.expect("read");
        let resp = Response::decode(&buf).expect("decode");
        // Explicit opt-in — since 2026-07-03 the pool defaults
        // to "discard on drop"; only sockets that had a
        // successful frame exchange should recycle.
        pooled.mark_clean();
        (resp, local)
    }

    #[tokio::test]
    async fn sequential_acquire_reuses_stream() {
        let _serial = pool_test_lock();
        clear();
        let (addr, _store, _handle) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn_node");
        let addr_str = addr.to_string();

        let (r1, local1) = ping(&addr_str).await;
        assert!(matches!(r1, Response::Pong));
        let (r2, local2) = ping(&addr_str).await;
        assert!(matches!(r2, Response::Pong));

        // Second acquire pulled the same socket out of the pool — so
        // its local port matches the first call's.
        assert_eq!(
            local1, local2,
            "second RPC should reuse the first connection's local endpoint"
        );

        let (addrs, idle) = stats();
        assert_eq!(addrs, 1, "exactly one address tracked");
        assert_eq!(idle, 1, "one idle stream after both calls released");
        clear();
    }

    #[tokio::test]
    async fn poison_drops_stream() {
        let _serial = pool_test_lock();
        clear();
        let (addr, _store, _handle) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn_node");
        let addr_str = addr.to_string();

        {
            let mut p = acquire(&addr_str).await.expect("acquire");
            write_frame(&mut p, &Request::Ping.encode())
                .await
                .expect("write");
            let _ = read_frame(&mut p).await.expect("read");
            p.poison();
        }

        let (_addrs, idle) = stats();
        assert_eq!(idle, 0, "poisoned stream must not be returned to pool");
        clear();
    }

    #[tokio::test]
    async fn expired_entries_are_dropped() {
        let _serial = pool_test_lock();
        clear();
        let _guard = EnvGuard::set("HOLOFS_POOL_IDLE_SECS", "0");
        let (addr, _store, _handle) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn_node");
        let addr_str = addr.to_string();

        let (r1, local1) = ping(&addr_str).await;
        assert!(matches!(r1, Response::Pong));
        // Force the cached entry to look expired immediately.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (r2, local2) = ping(&addr_str).await;
        assert!(matches!(r2, Response::Pong));

        assert_ne!(
            local1, local2,
            "expired pool entry should have been discarded and a fresh socket dialed"
        );

        clear();
    }

    #[tokio::test]
    async fn disable_flag_bypasses_pool() {
        let _serial = pool_test_lock();
        clear();
        let _guard = EnvGuard::set("HOLOFS_POOL_DISABLE", "1");
        let (addr, _store, _handle) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("spawn_node");
        let addr_str = addr.to_string();

        let (r1, local1) = ping(&addr_str).await;
        assert!(matches!(r1, Response::Pong));
        let (r2, local2) = ping(&addr_str).await;
        assert!(matches!(r2, Response::Pong));

        assert_ne!(
            local1, local2,
            "disabled pool must dial a fresh socket every time"
        );
        let (_addrs, idle) = stats();
        assert_eq!(idle, 0, "disabled pool keeps nothing");

        clear();
    }
}
