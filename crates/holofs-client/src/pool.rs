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
//! * `HOLOFS_POOL_PER_NODE` — max idle entries per addr (default `32`).
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
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use holofs_storage::identity::{NodeIdentity, PubKey};
use holofs_storage::node_service::perform_bilateral_handshake_as_client;

use crate::transport::{self, TransportStream};

/// Client-side authentication material for the P0.3b bilateral
/// handshake. Installed process-wide via [`set_client_auth`] at
/// gateway bootstrap; every fresh pool-dial afterwards runs the
/// handshake before the stream is handed out. Reused pooled
/// connections skip the handshake — the TCP peer is the same
/// authenticated party that completed it the first time.
///
/// `None` disables the handshake entirely (pre-P0.3b behaviour;
/// nothing changes). Setting this to `Some` while any nodes in
/// `node_pubkeys` run in `--client-whitelist` strict mode requires
/// `identity.pubkey()` to be on their whitelist — otherwise the
/// handshake fails and the pool surfaces `ConnectionAborted`.
pub struct ClientAuthConfig {
    /// Gateway's own Ed25519 identity — used to sign the node's
    /// counter-nonce during the handshake.
    pub identity: NodeIdentity,
    /// Map from node addr (as the pool sees it, i.e. the string
    /// used with `acquire`) to that node's expected pubkey. Nodes
    /// absent from this map bypass the handshake — legacy nodes /
    /// nodes running in permissive mode.
    pub node_pubkeys: HashMap<String, PubKey>,
}

/// Install process-wide client-auth material. Called by
/// `holofs_web::bootstrap` after loading `gateway_identity.key`
/// and the signed cluster whitelist. Passing `None` clears any
/// previously-installed config so tests can reset state.
pub fn set_client_auth(cfg: Option<Arc<ClientAuthConfig>>) {
    let slot = client_auth_slot();
    if let Ok(mut w) = slot.write() {
        *w = cfg;
    }
}

fn client_auth_slot() -> &'static RwLock<Option<Arc<ClientAuthConfig>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<ClientAuthConfig>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

fn current_client_auth() -> Option<Arc<ClientAuthConfig>> {
    client_auth_slot().read().ok().and_then(|r| r.clone())
}

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

// Pool tuning is snapshotted at first access via `OnceLock` and never
// re-read in release builds. Before this every RPC path hit
// `std::env::var` up to three times (disabled + idle_ttl +
// per_node_cap) — libc's env lookup takes the global env mutex, which
// under 50-worker load showed up as measurable contention on
// `acquire`/`release`. The env knobs are documented as boot-time so
// freezing them at first use matches the user-facing contract.
//
// Under `cfg(test)`, every call re-reads instead — pool tests
// (`sequential_acquire_reuses_stream`, `disabled_pool_dials_fresh`,
// …) flip the same env inside the same process, and a cached first
// value would leak between tests despite the `pool_test_lock` serial
// guard.
fn per_node_cap() -> usize {
    #[cfg(test)]
    {
        return read_per_node_cap();
    }
    #[cfg(not(test))]
    {
        static V: OnceLock<usize> = OnceLock::new();
        *V.get_or_init(read_per_node_cap)
    }
}

fn read_per_node_cap() -> usize {
    std::env::var("HOLOFS_POOL_PER_NODE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(32)
}

fn idle_ttl() -> Duration {
    #[cfg(test)]
    {
        return read_idle_ttl();
    }
    #[cfg(not(test))]
    {
        static V: OnceLock<Duration> = OnceLock::new();
        *V.get_or_init(read_idle_ttl)
    }
}

fn read_idle_ttl() -> Duration {
    let secs: u64 = std::env::var("HOLOFS_POOL_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

fn disabled() -> bool {
    #[cfg(test)]
    {
        return read_disabled();
    }
    #[cfg(not(test))]
    {
        static V: OnceLock<bool> = OnceLock::new();
        *V.get_or_init(read_disabled)
    }
}

fn read_disabled() -> bool {
    matches!(
        std::env::var("HOLOFS_POOL_DISABLE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Acquire a connection to `addr`, reusing an idle one if available.
///
/// Returns a [`Pooled`] guard. The underlying stream is returned to the
/// pool when the guard is dropped, unless [`Pooled::poison`] was called.
pub async fn acquire(addr: &str) -> io::Result<Pooled> {
    if disabled() {
        let mut stream = transport::connect(addr).await?;
        handshake_if_configured(addr, &mut stream).await?;
        // keep_on_drop=false so a disabled-pool acquire never leaks
        // a TCP connection into the cache after the env knob flips.
        return Ok(Pooled::detached(addr.to_string(), stream));
    }
    if let Some(stream) = pop_fresh(addr) {
        // Reused connection already handshake'd on first dial; skip.
        return Ok(Pooled::new(addr.to_string(), stream, true));
    }
    let mut stream = transport::connect(addr).await?;
    handshake_if_configured(addr, &mut stream).await?;
    Ok(Pooled::new(addr.to_string(), stream, false))
}

/// Acquire a freshly-dialed connection, bypassing the idle pool.
/// Used on the [`rpc`] retry path (B12): the first attempt may have
/// failed on a stale keepalive socket the OS hadn't yet reaped, and
/// `pop_fresh` would happily hand out the next idle sibling from the
/// same LIFO queue — very likely equally dead. Freshly dialing on
/// the retry breaks that streak.
pub async fn acquire_fresh(addr: &str) -> io::Result<Pooled> {
    let mut stream = transport::connect(addr).await?;
    handshake_if_configured(addr, &mut stream).await?;
    if disabled() {
        return Ok(Pooled::detached(addr.to_string(), stream));
    }
    Ok(Pooled::new(addr.to_string(), stream, false))
}

/// If [`set_client_auth`] has been called AND we know the expected
/// pubkey for `addr`, run the P0.3b bilateral handshake on the
/// freshly-dialed stream. Failure propagates as
/// `ConnectionAborted` so the caller can retry (or fail cleanly).
///
/// Nodes not present in `node_pubkeys` skip the handshake — they're
/// either legacy peers or running in permissive mode. This lets a
/// gateway mix trusted-cluster and legacy nodes during rollout
/// without a big-bang cutover.
async fn handshake_if_configured(addr: &str, stream: &mut TransportStream) -> io::Result<()> {
    let Some(cfg) = current_client_auth() else {
        return Ok(());
    };
    let Some(expected) = cfg.node_pubkeys.get(addr).copied() else {
        return Ok(());
    };
    perform_bilateral_handshake_as_client(stream, &cfg.identity, &expected)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, format!("handshake {addr}: {e}")))
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
/// **Drop semantics**rework): the default is to
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
/// `UnexpectedResponse` (see the—
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
        // Explicit opt-in — sincethe pool defaults
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

    // === P0.3b: handshake-aware pool ==================================

    #[tokio::test]
    async fn acquire_runs_handshake_when_client_auth_configured() {
        // End-to-end: node with `--client-whitelist`-style enforcement
        // (Some(wl)) + gateway with client_auth (identity + node
        // pubkey map) → acquire() completes handshake transparently
        // and subsequent Ping/Pong flows normally.
        use holofs_storage::identity::NodeIdentity;
        use holofs_storage::node_service::spawn_node_persistent_with_tls_and_whitelist;
        use holofs_storage::whitelist::{Whitelist, WhitelistEntry};

        let _serial = pool_test_lock();
        clear();
        set_client_auth(None); // reset any prior test's install

        // Build cluster whitelist: contains the client identity so
        // the node accepts our handshake.
        let admin = NodeIdentity::generate();
        let client_id = NodeIdentity::generate();
        let client_pk = client_id.pubkey();
        let ingress_wl = Arc::new(Whitelist::sign(
            vec![WhitelistEntry {
                addr: String::new(),
                pubkey: client_pk,
                zone: 0,
            }],
            &admin,
        ));

        let dir = std::env::temp_dir().join(format!(
            "pool-handshake-happy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (bound, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(ingress_wl),
        )
        .await
        .unwrap();

        // Load the node's own identity so we can tell the client
        // what pubkey to expect.
        let node_id =
            NodeIdentity::load_or_create(dir.join("identity.key")).unwrap();
        let mut node_pubkeys = HashMap::new();
        node_pubkeys.insert(bound.to_string(), node_id.pubkey());
        set_client_auth(Some(Arc::new(ClientAuthConfig {
            identity: client_id,
            node_pubkeys,
        })));

        // acquire runs handshake under the hood; ping/pong works.
        let (resp, _) = ping(&bound.to_string()).await;
        assert!(matches!(resp, Response::Pong));

        // Cleanup so other tests in this file see a fresh slot.
        set_client_auth(None);
        clear();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn acquire_skips_handshake_when_addr_not_in_pubkey_map() {
        // Client auth is installed BUT this addr is not on the
        // gateway's pubkey map (mixed rollout: some nodes still in
        // permissive mode). Handshake is skipped, RPC works over the
        // legacy path unchanged.
        use holofs_storage::identity::NodeIdentity;

        let _serial = pool_test_lock();
        clear();
        set_client_auth(None);

        // Install auth that references a bogus addr so the real
        // node lookup returns None.
        set_client_auth(Some(Arc::new(ClientAuthConfig {
            identity: NodeIdentity::generate(),
            node_pubkeys: HashMap::from([
                ("192.0.2.1:9999".to_string(), [0xAAu8; 32]),
            ]),
        })));

        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into())
            .await
            .unwrap();
        let (resp, _) = ping(&addr.to_string()).await;
        assert!(matches!(resp, Response::Pong));

        set_client_auth(None);
        clear();
    }

    #[tokio::test]
    async fn acquire_fails_when_gateway_not_whitelisted() {
        // Gateway identity is NOT on the node's ingress whitelist →
        // handshake rejected → acquire() surfaces
        // ConnectionAborted.
        use holofs_storage::identity::NodeIdentity;
        use holofs_storage::node_service::spawn_node_persistent_with_tls_and_whitelist;
        use holofs_storage::whitelist::{Whitelist, WhitelistEntry};

        let _serial = pool_test_lock();
        clear();
        set_client_auth(None);

        let admin = NodeIdentity::generate();
        let authorised = NodeIdentity::generate(); // NOT our client
        let intruder_id = NodeIdentity::generate();
        let ingress_wl = Arc::new(Whitelist::sign(
            vec![WhitelistEntry {
                addr: String::new(),
                pubkey: authorised.pubkey(),
                zone: 0,
            }],
            &admin,
        ));

        let dir = std::env::temp_dir().join(format!(
            "pool-handshake-reject-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (bound, _store, _h) = spawn_node_persistent_with_tls_and_whitelist(
            (Ipv4Addr::LOCALHOST, 0).into(),
            &dir,
            None,
            Some(ingress_wl),
        )
        .await
        .unwrap();

        let node_id =
            NodeIdentity::load_or_create(dir.join("identity.key")).unwrap();
        let mut node_pubkeys = HashMap::new();
        node_pubkeys.insert(bound.to_string(), node_id.pubkey());
        set_client_auth(Some(Arc::new(ClientAuthConfig {
            identity: intruder_id,
            node_pubkeys,
        })));

        let err = match acquire(&bound.to_string()).await {
            Ok(_) => panic!("acquire must fail when gateway pubkey is not on ingress whitelist"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert!(
            err.to_string().contains("handshake"),
            "expected handshake error, got: {err}"
        );

        set_client_auth(None);
        clear();
        std::fs::remove_dir_all(&dir).ok();
    }
}
