//! Node daemon. Listens on TCP (optionally TLS/mTLS), stores shards
//! (in-memory or on disk).
//!
//! Run:
//! ```sh
//! # Plain TCP
//! holofs-node 127.0.0.1:5000
//! holofs-node 127.0.0.1:5000 --storage ./data/node-1
//!
//! # TLS: encrypted wire, server presents its own cert. Any TCP peer
//! # can still connect (no client-auth). Files are PEM-encoded.
//! holofs-node 0.0.0.0:5000 --storage ./data/node-1 \
//!     --tls-cert /etc/holofs/node.crt \
//!     --tls-key  /etc/holofs/node.key \
//!     --tls-ca   /etc/holofs/ca.crt
//!
//! # mTLS: same as TLS, plus the server rejects any connection whose
//! # client certificate is not signed by the CA. This is the minimum
//! # data-plane authentication contract for `trusted_cluster` prod
//! # (PRODUCTION-READINESS §P0.3).
//! holofs-node 0.0.0.0:5000 --storage ./data/node-1 \
//!     --tls-cert /etc/holofs/node.crt \
//!     --tls-key  /etc/holofs/node.key \
//!     --tls-ca   /etc/holofs/ca.crt \
//!     --mtls
//! ```
//!
//! With `--storage <dir>` the node keeps shards and the Ed25519 keypair
//! in `dir`. On restart the index is rebuilt by scanning files, the
//! identity stays the same. The pubkey (for the whitelist) is printed
//! in hex at startup.
//!
//! TLS + in-memory (no `--storage`) is intentionally rejected: the
//! only supported in-memory path is embedded mode inside `holofs-web`.
//! Standalone daemons that want TLS need persistent storage.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use holofs_core::hash::hex;
use holofs_storage::node_service::{spawn_node, spawn_node_persistent_with_tls};
use holofs_storage::tls::TlsMaterial;

/// Parsed CLI, ready to hand to the async main.
struct Args {
    addr: SocketAddr,
    storage: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    tls_ca: Option<PathBuf>,
    mtls: bool,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut addr_str: Option<String> = None;
    let mut storage: Option<String> = None;
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut tls_ca: Option<PathBuf> = None;
    let mut mtls = false;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--storage" => {
                storage = Some(take_val(&raw, &mut i, "--storage")?);
            }
            "--tls-cert" => {
                tls_cert = Some(PathBuf::from(take_val(&raw, &mut i, "--tls-cert")?));
            }
            "--tls-key" => {
                tls_key = Some(PathBuf::from(take_val(&raw, &mut i, "--tls-key")?));
            }
            "--tls-ca" => {
                tls_ca = Some(PathBuf::from(take_val(&raw, &mut i, "--tls-ca")?));
            }
            "--mtls" => {
                mtls = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other if addr_str.is_none() => {
                addr_str = Some(other.to_string());
                i += 1;
            }
            other => {
                return Err(format!("unexpected argument: {other}"));
            }
        }
    }
    let addr_str = addr_str.unwrap_or_else(|| "127.0.0.1:5000".to_string());
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|e| format!("bad address {addr_str:?}: {e}"))?;
    Ok(Args {
        addr,
        storage,
        tls_cert,
        tls_key,
        tls_ca,
        mtls,
    })
}

fn take_val(raw: &[String], i: &mut usize, name: &str) -> Result<String, String> {
    let next = *i + 1;
    if next >= raw.len() {
        return Err(format!("{name} requires a value"));
    }
    let v = raw[next].clone();
    *i = next + 1;
    Ok(v)
}

/// Assemble the rustls [`ServerConfig`] from CLI flags, or return
/// `Ok(None)` for plain TCP. Rejects half-set combinations (e.g.
/// `--tls-cert` without `--tls-key`) with a helpful message.
fn build_tls_config(args: &Args) -> Result<Option<Arc<rustls::ServerConfig>>, String> {
    let any = args.tls_cert.is_some() || args.tls_key.is_some() || args.tls_ca.is_some();
    if !any && !args.mtls {
        return Ok(None);
    }
    let cert = args
        .tls_cert
        .as_ref()
        .ok_or_else(|| "--tls-cert required when any TLS flag is set".to_string())?;
    let key = args
        .tls_key
        .as_ref()
        .ok_or_else(|| "--tls-key required when any TLS flag is set".to_string())?;
    let ca = args
        .tls_ca
        .as_ref()
        .ok_or_else(|| "--tls-ca required when any TLS flag is set".to_string())?;
    // Runtime path never needs the CA private key — verification only
    // uses the CA cert. Passing `None` matches `TlsMaterial::load`'s
    // "verifier-only" contract (see its docstring).
    let material = TlsMaterial::load(cert, key, ca, None)
        .map_err(|e| format!("load TLS material: {e}"))?;
    let cfg = material
        .server_config(args.mtls)
        .map_err(|e| format!("build server config: {e}"))?;
    Ok(Some(cfg))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            print_usage();
            std::process::exit(2);
        }
    };
    let tls = match build_tls_config(&args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // TLS + in-memory would need a spawn_node_with_tls variant we
    // haven't added. Standalone daemons that want TLS must run
    // persistent so this combo is a fast fail with a useful message
    // rather than a silent fallback to plain TCP.
    if tls.is_some() && args.storage.is_none() {
        eprintln!(
            "TLS/mTLS requires --storage: standalone daemon TLS is only \
             supported in persistent mode. For in-memory + TLS use \
             embedded mode via holofs-web."
        );
        std::process::exit(2);
    }

    let (bound, handle, pubkey_hex, wire_mode) = if let Some(dir) = &args.storage {
        let (a, _s, h) =
            spawn_node_persistent_with_tls(args.addr, dir.as_str(), tls.clone()).await?;
        let id = holofs_storage::identity::NodeIdentity::load_or_create(
            std::path::Path::new(dir).join("identity.key"),
        )?;
        let pk = hex(&id.pubkey());
        let mode = match (tls.is_some(), args.mtls) {
            (false, _) => "plain-tcp",
            (true, false) => "tls",
            (true, true) => "mtls",
        };
        (a, h, pk, mode)
    } else {
        let (a, _s, h) = spawn_node(args.addr).await?;
        // In-memory: identity is generated randomly inside spawn_node
        // and is not exposed. We print "<ephemeral>" as a marker that
        // the pubkey is new each run.
        (a, h, "<ephemeral>".to_string(), "plain-tcp")
    };
    eprintln!("holofs-node addr={bound} pubkey={pubkey_hex} wire={wire_mode}");
    handle.await.ok();
    Ok(())
}

fn print_usage() {
    eprintln!(
        "usage: holofs-node [ADDR] [--storage DIR]\n\
         \x20              [--tls-cert PATH --tls-key PATH --tls-ca PATH] [--mtls]\n\
         \n\
         ADDR         host:port to bind (default 127.0.0.1:5000)\n\
         --storage    directory for shards and identity.key (persistent mode)\n\
         \n\
         --tls-cert   PEM leaf certificate presented to peers (server-side)\n\
         --tls-key    PEM private key matching --tls-cert\n\
         --tls-ca     PEM CA certificate trust root (also used as client-CA under --mtls)\n\
         --mtls       require + verify client cert signed by --tls-ca\n\
         \n\
         Notes:\n\
         \x20 - --tls-cert / --tls-key / --tls-ca must all be set together.\n\
         \x20 - TLS/mTLS requires --storage (persistent mode).\n\
         \x20 - No TLS flags → plain TCP, same as before."
    );
}
