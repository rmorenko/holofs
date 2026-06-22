//! Node daemon. Listens on TCP, stores shards (in-memory or on disk).
//!
//! Run:
//! ```sh
//! holofs-node 127.0.0.1:5000
//! holofs-node 127.0.0.1:5000 --storage ./data/node-1
//! ```
//!
//! With `--storage <dir>` the node keeps shards and the Ed25519 keypair in `dir`.
//! On restart the index is rebuilt by scanning files, the identity stays the same.
//! The pubkey (for the whitelist) is printed in hex at startup.

use std::net::SocketAddr;

use holofs_core::hash::hex;
use holofs_storage::node_service::{spawn_node, spawn_node_persistent};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut addr_str: Option<String> = None;
    let mut storage: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--storage" => {
                if i + 1 >= args.len() {
                    eprintln!("--storage requires a path");
                    std::process::exit(2);
                }
                storage = Some(args[i + 1].clone());
                i += 2;
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other if addr_str.is_none() => {
                addr_str = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("unexpected argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    let addr_str = addr_str.unwrap_or_else(|| "127.0.0.1:5000".to_string());
    let addr: SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bad address {addr_str:?}: {e}");
            std::process::exit(2);
        }
    };

    let (bound, handle, pubkey_hex) = if let Some(dir) = storage {
        let (a, _s, h) = spawn_node_persistent(addr, &dir).await?;
        // Identity is loaded inside spawn_node_persistent from dir/identity.key.
        // To print the pubkey we open NodeIdentity explicitly.
        let id = holofs_storage::identity::NodeIdentity::load_or_create(
            std::path::Path::new(&dir).join("identity.key"),
        )?;
        let pk = hex(&id.pubkey());
        (a, h, pk)
    } else {
        let (a, _s, h) = spawn_node(addr).await?;
        // In-memory: identity is generated randomly inside spawn_node and is not
        // exposed. We print "<ephemeral>" as a marker that the pubkey is new each run.
        (a, h, "<ephemeral>".to_string())
    };
    eprintln!("holofs-node addr={bound} pubkey={pubkey_hex}");
    handle.await.ok();
    Ok(())
}

fn print_usage() {
    eprintln!(
        "usage: holofs-node [ADDR] [--storage DIR]\n\
         \n\
         ADDR        host:port to bind (default 127.0.0.1:5000)\n\
         --storage   directory for shards and identity.key (persistent mode)"
    );
}
