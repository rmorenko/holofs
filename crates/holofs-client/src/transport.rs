//! Transport layer for client→node connections.
//!
//! Stage 6 introduced optional TLS on the wire protocol. The actual switch
//! is a process-wide setting: at bootstrap the binary calls
//! [`set_tls_config`] with either `None` (plain TCP — legacy default) or
//! `Some(Arc<ClientConfig>)` (TLS, optionally with a client cert for mTLS).
//!
//! Every helper that needs a connection calls [`connect`] which returns a
//! type-erased `TransportStream` so the wire helpers
//! (`holofs_wire::{read_frame, write_frame}`) work over either kind.
//!
//! The global is initialised at most once per process and read on every
//! connection. There's no atomic swap-after-init — TLS is decided once at
//! startup and stays the same for the life of the binary.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use holofs_storage::tls::{server_name_for, TlsBuildError};

/// Set the process-wide TLS configuration. Pass `None` to use plain TCP.
/// Idempotent: calling twice with the same value is a no-op; calling with a
/// conflicting value panics so misconfigured deployments fail loudly.
pub fn set_tls_config(cfg: Option<Arc<rustls::ClientConfig>>) {
    // We model the global as `Option<Arc<ClientConfig>>` so the absence of
    // TLS is positively asserted (vs. "not initialised yet").
    static CONFIG: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    if CONFIG.set(cfg).is_err() {
        // Already set — reuse the global state struct's slot.
        let existing = CONFIG.get().expect("OnceLock set above");
        let now: Option<Arc<rustls::ClientConfig>> = current_arc();
        match (existing, &now) {
            (None, None) => {}
            (Some(a), Some(b)) if Arc::ptr_eq(a, b) => {}
            _ => panic!("holofs_client::transport: TLS config already initialised"),
        }
    } else {
        store_current(CONFIG.get().expect("just set").clone());
    }
}

fn store_current(cfg: Option<Arc<rustls::ClientConfig>>) {
    let mut guard = CURRENT.write().expect("transport cfg poisoned");
    *guard = cfg;
}

fn current_arc() -> Option<Arc<rustls::ClientConfig>> {
    CURRENT.read().expect("transport cfg poisoned").clone()
}

static CURRENT: std::sync::RwLock<Option<Arc<rustls::ClientConfig>>> =
    std::sync::RwLock::new(None);

/// Open a connection to `addr` honoring the global TLS config. On a TLS
/// build the SNI is derived from the host portion of `addr` (IPv4/IPv6 → IP,
/// hostname → DNS).
pub async fn connect(addr: &str) -> io::Result<TransportStream> {
    let tcp = TcpStream::connect(addr).await?;
    match current_arc() {
        None => Ok(TransportStream::Plain(tcp)),
        Some(cfg) => {
            let connector = TlsConnector::from(cfg);
            let sni =
                server_name_for(addr).map_err(|e: TlsBuildError| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let tls = connector
                .connect(sni, tcp)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()))?;
            Ok(TransportStream::Tls(Box::new(tls)))
        }
    }
}

/// Either a plain TCP stream or a rustls TLS stream sitting on one. Both
/// arms implement `AsyncRead + AsyncWrite + Unpin` so the wire helpers
/// don't care which is which.
pub enum TransportStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            TransportStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TransportStream::Plain(s) => Pin::new(s).poll_write(cx, data),
            TransportStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, data),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Plain(s) => Pin::new(s).poll_flush(cx),
            TransportStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            TransportStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
