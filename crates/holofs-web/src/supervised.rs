//! N2 — supervised background-task helper.
//!
//! [`supervised_spawn`] wraps a fire-and-forget async task so that a
//! panic no longer silently kills the whole background loop. Instead:
//!
//! * The inner task is spawned as a **child** `tokio::task`. `JoinHandle`
//!   captures panics via [`tokio::task::JoinError::is_panic`] — no
//!   `AssertUnwindSafe` gymnastics needed on the caller side.
//! * On panic we log the task name at ERROR and sleep an exponential
//!   backoff (1s → 2 → 4 → 8 → 16 → 30s ceiling), then restart. The
//!   backoff resets to 1s after a normal (non-panic) return.
//! * The supervisor loop respects the shared [`CancellationToken`]: a
//!   shutdown cancels the backoff sleep, drops the child, and returns
//!   the outer JoinHandle.
//!
//! Motivation: pre-N2 the health monitor and PoR auditor advertised
//! "suppresses per-tick panics" in their docs but never actually
//! called `catch_unwind` — a panic anywhere under `tick_once` would
//! leak up and silently kill the whole loop, and the operator would
//! only notice hours later when `holo_scrub_runs_total` stopped
//! moving. Wrapping the three long-running loops in `supervised_spawn`
//! makes any such crash loud and self-healing.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Initial sleep before the first restart after a panic. Doubles on
/// each subsequent panic until [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling for the exponential-backoff sleep between restarts. Chosen
/// so a persistently-crashing task doesn't hammer the log every 30s
/// on the low end while still recovering within one poll interval of
/// any transient blip.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Spawn a supervised background loop. Returns the JoinHandle of the
/// supervisor itself — awaiting it drains cleanly once `shutdown` is
/// cancelled.
///
/// `make_fut` is a factory closure invoked on every restart so the
/// task gets a fresh future each time. State that must survive across
/// restarts belongs to captured `Arc`s in the closure.
pub fn supervised_spawn<F, Fut>(
    name: &'static str,
    shutdown: CancellationToken,
    restarts_counter: Arc<AtomicU64>,
    make_fut: F,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        info!(task = name, "supervised task started");
        loop {
            if shutdown.is_cancelled() {
                info!(task = name, "supervised task exiting (shutdown)");
                return;
            }
            // Spawn the inner future as its own task so panics surface
            // via JoinError rather than aborting the supervisor. The
            // supervisor keeps the JoinHandle alive so the child can't
            // outlive us.
            let child = tokio::spawn(make_fut());
            let outcome = tokio::select! {
                res = child => res,
                _ = shutdown.cancelled() => {
                    // Parent shutdown: tokio drops the abort-on-drop
                    // handle when we return, cancelling the child.
                    info!(task = name, "supervised task exiting (shutdown)");
                    return;
                }
            };
            match outcome {
                Ok(()) => {
                    // Task returned normally. Two cases:
                    //   1. It returned because shutdown fired — the
                    //      cancel check at loop top handles it.
                    //   2. It returned voluntarily without a shutdown
                    //      — surprising for a run-forever loop; log
                    //      and restart with the reset backoff.
                    if shutdown.is_cancelled() {
                        info!(task = name, "supervised task exiting (shutdown)");
                        return;
                    }
                    restarts_counter.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        task = name,
                        total_restarts = restarts_counter.load(Ordering::Relaxed),
                        "task returned without shutdown; restarting immediately"
                    );
                    backoff = INITIAL_BACKOFF;
                }
                Err(e) if e.is_panic() => {
                    // JoinError does not deref the panic payload
                    // safely into a Display, so we don't try to
                    // extract the message. The panic backtrace is
                    // already on stderr from the tokio runtime; we
                    // just record the fact + our decision.
                    restarts_counter.fetch_add(1, Ordering::Relaxed);
                    error!(
                        task = name,
                        backoff_secs = backoff.as_secs(),
                        total_restarts = restarts_counter.load(Ordering::Relaxed),
                        "supervised task panicked; restarting after backoff"
                    );
                    tokio::select! {
                        _ = shutdown.cancelled() => {
                            info!(task = name, "supervised task exiting (shutdown)");
                            return;
                        }
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(e) => {
                    // Cancelled by explicit `handle.abort()` from
                    // somewhere else — treat as shutdown.
                    info!(task = name, cancelled = %e, "supervised child cancelled; exiting");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn restarts_after_panic() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let restarts = Arc::new(AtomicU64::new(0));
        let shutdown = CancellationToken::new();
        let a = Arc::clone(&attempts);
        let sd = shutdown.clone();

        let handle = supervised_spawn(
            "test-panic",
            shutdown.clone(),
            Arc::clone(&restarts),
            move || {
                let a = Arc::clone(&a);
                let sd = sd.clone();
                async move {
                    let n = a.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        panic!("boom {n}");
                    }
                    // 3rd attempt: settle, then wait for shutdown so
                    // the supervisor doesn't spin restarting a healthy
                    // task.
                    sd.cancelled().await;
                }
            },
        );

        // Give a few backoff cycles a chance to run.
        tokio::time::sleep(Duration::from_millis(3200)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        assert!(
            attempts.load(Ordering::SeqCst) >= 3,
            "expected at least 3 attempts (2 panics + healthy run), got {}",
            attempts.load(Ordering::SeqCst)
        );
        // The two panics each bump the restart counter; the healthy
        // 3rd attempt exits via the shutdown token and doesn't count
        // as a restart because the supervisor sees shutdown-cancelled
        // and exits before the "task returned normally" branch fires.
        assert!(
            restarts.load(Ordering::Relaxed) >= 2,
            "expected >= 2 restarts recorded, got {}",
            restarts.load(Ordering::Relaxed)
        );
    }

    #[tokio::test]
    async fn shutdown_exits_promptly() {
        let restarts = Arc::new(AtomicU64::new(0));
        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        let handle = supervised_spawn(
            "test-shutdown",
            shutdown.clone(),
            Arc::clone(&restarts),
            move || {
                let sd = sd.clone();
                async move {
                    sd.cancelled().await;
                }
            },
        );
        // Give it a beat to enter the child future.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(joined.is_ok(), "supervisor did not exit within 1s of shutdown");
        assert_eq!(
            restarts.load(Ordering::Relaxed),
            0,
            "clean shutdown should not increment restarts counter"
        );
    }
}
