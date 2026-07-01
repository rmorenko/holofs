//! Shared test-only utilities for holofs.
//!
//! Three crates in the workspace (`holofs-client`, `holofs-cluster::audit`,
//! `holofs-cluster::monitor`) used to carry near-identical copies of
//! these helpers. Each copy drifted slightly across the codebase, and
//! every fix had to be replicated three times. This crate is the
//! single source of truth.
//!
//! Consume via `dev-dependencies`:
//!
//! ```toml
//! [dev-dependencies]
//! holofs-testutils.workspace = true
//! ```
//!
//! Two primitives are exposed:
//!
//! - [`DisablePool`] — RAII guard that sets `HOLOFS_POOL_DISABLE=1` for
//!   the test's scope, holds an inter-test mutex, and restores the
//!   previous value on drop. Required for any test that drives
//!   `holofs_client::pool` or its consumers — without it, the pool's
//!   keepalive can hand back stale sockets across tests.
//!
//! - [`spawn_mock_node`] — tiny multi-shot TCP listener that answers
//!   every accepted connection with the same configured
//!   `holofs_wire::Response`. Used to exercise wire-protocol callers
//!   (audit_shard, list_node_hashes, purge_node_by_hash, discover_live,
//!   monitor::tick_once) without spinning up a real storage node.

use std::sync::{Mutex, MutexGuard, OnceLock};

use holofs_wire::Response;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// RAII guard that:
/// 1. Holds a process-wide mutex so concurrent pool-touching tests
///    serialise (the keepalive pool is global; tests that disable
///    it can't overlap with tests that assert reuse semantics).
/// 2. Sets `HOLOFS_POOL_DISABLE=1` so every `pool::acquire` dials
///    a fresh socket for the test's lifetime.
/// 3. Restores the previous env-var value on drop.
///
/// Hold one of these at the top of any `#[tokio::test]` that uses
/// `holofs_client::pool` (directly or via `audit_shard` / RPC helpers).
pub struct DisablePool {
    _lock: MutexGuard<'static, ()>,
    old: Option<String>,
}

impl DisablePool {
    /// Acquire the guard. Blocks if another test is already holding
    /// it. The lock is `std::sync::Mutex`, not async — the contention
    /// window is microseconds (just the env-var bookkeeping), so an
    /// async mutex would buy nothing here.
    #[must_use]
    pub fn new() -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let old = std::env::var("HOLOFS_POOL_DISABLE").ok();
        std::env::set_var("HOLOFS_POOL_DISABLE", "1");
        Self { _lock: lock, old }
    }
}

impl Drop for DisablePool {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => std::env::set_var("HOLOFS_POOL_DISABLE", v),
            None => std::env::remove_var("HOLOFS_POOL_DISABLE"),
        }
    }
}

/// Spawn a TCP listener on an ephemeral 127.0.0.1 port that answers
/// every accepted connection with the same `response`, encoded via
/// the wire protocol's standard `[u32 len BE][payload]` framing.
///
/// The listener keeps accepting until the returned [`JoinHandle`] is
/// dropped (which closes the listener and tears down the accept
/// task). Each connection runs in its own spawned task and loops
/// reading frames until the client closes — so the pool can reuse
/// the same socket across multiple RPCs against the mock node.
///
/// Returns `(addr, handle)`. `addr` is the bound `127.0.0.1:NNNN`
/// string the caller passes to `holofs_client` RPC helpers.
pub async fn spawn_mock_node(response: Response) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener
        .local_addr()
        .expect("local_addr")
        .to_string();
    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let resp_bytes = response.encode();
            tokio::spawn(async move {
                loop {
                    let mut len_buf = [0u8; 4];
                    if sock.read_exact(&mut len_buf).await.is_err() {
                        return;
                    }
                    let len = u32::from_be_bytes(len_buf) as usize;
                    let mut req = vec![0u8; len];
                    if sock.read_exact(&mut req).await.is_err() {
                        return;
                    }
                    let reply_len = (resp_bytes.len() as u32).to_be_bytes();
                    if sock.write_all(&reply_len).await.is_err() {
                        return;
                    }
                    if sock.write_all(&resp_bytes).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, handle)
}
