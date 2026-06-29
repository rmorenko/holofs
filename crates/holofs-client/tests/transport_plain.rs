//! Coverage P2: Plain-TCP path through `holofs_client::transport`.
//!
//! `tls_roundtrip.rs` already covers the TLS arm of `connect()` /
//! `TransportStream::Tls(..)`. This file deliberately does NOT call
//! `set_tls_config`, so `connect()` falls through to the
//! `TransportStream::Plain(TcpStream)` arm — exercising the Plain
//! poll_read/poll_write/poll_flush/poll_shutdown delegation that
//! never runs under the TLS test (a fresh test binary = fresh
//! OnceLock, so the global TLS config really is `None` here).

use std::time::Duration;

use holofs_client::transport::{connect, TransportStream};
use holofs_wire::{read_frame, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// Spawn a plain-TCP echo server: reads one wire frame, writes it
/// back, then `shutdown`s. The `shutdown` matters — it forces the
/// client's `poll_shutdown` to actually do work, lifting that arm
/// of the AsyncWrite impl out of "never executed".
async fn spawn_plain_echo() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            tokio::spawn(async move {
                if let Ok(buf) = read_frame(&mut sock).await {
                    let _ = write_frame(&mut sock, &buf).await;
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn connect_without_tls_returns_plain_variant() {
    let addr = spawn_plain_echo().await;
    let s = tokio::time::timeout(Duration::from_secs(5), connect(&addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    assert!(
        matches!(s, TransportStream::Plain(_)),
        "expected TransportStream::Plain variant"
    );
}

#[tokio::test]
async fn plain_stream_round_trips_a_frame() {
    let addr = spawn_plain_echo().await;
    let mut s = connect(&addr).await.expect("connect");
    let payload = b"plain wire stays plain".to_vec();
    write_frame(&mut s, &payload).await.unwrap();
    let echoed = read_frame(&mut s).await.unwrap();
    assert_eq!(echoed, payload);
    // Explicit shutdown — exercises poll_shutdown on the Plain arm
    // (the receiver loop already shutdown its side; this completes
    // the half-close from our side).
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn connect_to_closed_port_surfaces_io_error() {
    // Bind+drop to grab a port that nobody listens on. `connect`'s
    // TcpStream::connect arm propagates the OS error directly — this
    // covers the `?` propagation in the `Ok(tcp)` early return path.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    let res = connect(&addr).await;
    assert!(res.is_err(), "expected error on closed port");
}
