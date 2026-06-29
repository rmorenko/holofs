//! Coverage C: TLS handshake + frame round-trip through
//! `holofs_client::transport::connect`.
//!
//! Lives as an integration test (its own process) because
//! `transport::set_tls_config` uses a `OnceLock` — a second call
//! with a different value panics. Running in a fresh process
//! sidesteps that constraint without touching the production API.

use std::sync::Arc;
use std::time::Duration;

use holofs_client::transport::{connect, set_tls_config, TransportStream};
use holofs_storage::tls::TlsMaterial;
use holofs_wire::{read_frame, write_frame};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Install the ring crypto provider exactly once for this test
/// binary. Without it, rustls 0.23's leaf-cert verifier rejects
/// signatures with `BadSignature` because no provider knows how to
/// check ECDSA-with-SHA256 (rcgen's default).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Idempotent: install_default returns Err if a provider was
        // already installed (e.g. by another test in the same
        // binary). We don't care which test "won" — only that some
        // ring provider is live before the first ClientConfig is
        // built. Without this call, rustls 0.23 rejects the leaf
        // cert with `BadSignature` because no provider knows how to
        // check ECDSA-with-SHA256 (rcgen's default leaf alg).
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build (or reuse) a self-signed CA + leaf with `127.0.0.1` in the
/// SAN list so the rustls `ServerName::IpAddress("127.0.0.1")` SNI
/// check passes. The material is cached for the lifetime of the
/// test binary because `set_tls_config` keeps the FIRST client
/// config it ever sees — a second test with a different CA would
/// see "BadSignature" when its server presents a leaf signed by a
/// CA that the first-installed client config doesn't trust.
fn shared_material() -> &'static TlsMaterial {
    use std::sync::OnceLock;
    static MAT: OnceLock<TlsMaterial> = OnceLock::new();
    MAT.get_or_init(|| {
        let (mat, _signer) = TlsMaterial::self_signed("holofs-test", &["127.0.0.1".into()])
            .expect("self-signed material");
        mat
    })
}

/// Bind a TLS-accepting TCP listener on an ephemeral port. The
/// listener loops accepting connections and, for each, reads one
/// wire frame and echoes it back. Returns the bound addr.
async fn spawn_tls_echo_server(mat: &TlsMaterial) -> String {
    let cfg = mat
        .server_config(false)
        .expect("server_config");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let acceptor = TlsAcceptor::from(cfg);
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let acc = acceptor.clone();
            tokio::spawn(async move {
                let mut tls = match acc.accept(sock).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                if let Ok(buf) = read_frame(&mut tls).await {
                    let _ = write_frame(&mut tls, &buf).await;
                }
            });
        }
    });
    addr
}

/// TLS handshake + one round-trip frame. Pins:
/// - `connect()` returns the Tls variant when set_tls_config is on
/// - the rustls handshake succeeds against the self-signed CA
/// - write_frame + read_frame work through the wrapped stream
/// - the byte payload is preserved end-to-end
#[tokio::test]
async fn tls_connect_then_echo_roundtrips_a_frame() {
    ensure_crypto_provider();
    let mat = shared_material();
    let client_cfg = mat.client_config(false).expect("client_config");
    set_tls_config(Some(Arc::clone(&client_cfg)));

    let server_addr = spawn_tls_echo_server(&mat).await;
    let mut s = tokio::time::timeout(Duration::from_secs(5), connect(&server_addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    // Make sure we actually got a TLS stream, not the plain fallback.
    assert!(
        matches!(s, TransportStream::Tls(_)),
        "expected TransportStream::Tls — set_tls_config seems to have been ignored"
    );

    let payload = vec![0xABu8; 1024];
    write_frame(&mut s, &payload).await.expect("write_frame");
    let echoed = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut s))
        .await
        .expect("read_frame timeout")
        .expect("read_frame");
    assert_eq!(echoed, payload, "echoed frame differs from sent");
}

/// set_tls_config called a second time with the SAME Arc must be a
/// no-op (idempotent). Calling with a *different* config panics — but
/// the OnceLock semantics make that hard to assert here without a
/// dedicated process, so this test only covers the idempotent path.
#[tokio::test]
async fn set_tls_config_is_idempotent_with_same_arc() {
    ensure_crypto_provider();
    let mat = shared_material();
    let cfg = mat.client_config(false).expect("client_config");
    // First call (may or may not be the very first in the binary —
    // the other test in this file already ran depending on order).
    set_tls_config(Some(Arc::clone(&cfg)));
    // Second call with the same Arc — must not panic, must not
    // change observable state.
    set_tls_config(Some(Arc::clone(&cfg)));
}
