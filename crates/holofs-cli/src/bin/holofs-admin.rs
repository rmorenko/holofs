//! Admin utility: generate an Ed25519 keypair and sign the cluster whitelist.
//!
//! ```sh
//! # generate an admin keypair
//! holofs-admin gen-key admin.key
//!
//! # print the pubkey (for clients to keep as a trust anchor)
//! holofs-admin pubkey admin.key
//!
//! # sign the whitelist
//! holofs-admin sign-whitelist \\
//!   --admin admin.key \\
//!   --node 127.0.0.1:5000=PUBKEY_HEX:0 \\
//!   --node 127.0.0.1:5001=PUBKEY_HEX:1 \\
//!   --out cluster.wl
//! ```
//!
//! `--node` format: `ADDR=PUBKEY_HEX:ZONE`. PUBKEY_HEX is taken from the
//! output of `holofs-node --storage <dir>` at first start.

use std::path::Path;

use holofs_core::hash::hex;
use holofs_storage::identity::{NodeIdentity, PUBKEY_LEN};
use holofs_storage::whitelist::{Whitelist, WhitelistEntry};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        std::process::exit(2);
    }
    match args[0].as_str() {
        "gen-key" => cmd_gen_key(&args[1..]),
        "pubkey" => cmd_pubkey(&args[1..]),
        "sign-whitelist" => cmd_sign_whitelist(&args[1..]),
        "verify-whitelist" => cmd_verify_whitelist(&args[1..]),
        "show-whitelist" => cmd_show_whitelist(&args[1..]),
        "--help" | "-h" | "help" => {
            print_usage();
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: holofs-admin <command> [args]\n\
\n\
commands:\n\
  gen-key <out.key>                     generate a new Ed25519 keypair\n\
  pubkey <key>                          print pubkey hex\n\
  sign-whitelist --admin <key> --node ADDR=PUBKEY_HEX:ZONE [--node …] --out <file>\n\
                                        sign the cluster whitelist\n\
  verify-whitelist <file> [--admin-pubkey HEX]\n\
                                        verify the signature (opt. against expected admin)\n\
  show-whitelist <file>                 print contents"
    );
}

fn cmd_gen_key(args: &[String]) {
    if args.len() != 1 {
        eprintln!("usage: gen-key <out.key>");
        std::process::exit(2);
    }
    let path = Path::new(&args[0]);
    if path.exists() {
        eprintln!("file {path:?} already exists — refusing");
        std::process::exit(1);
    }
    let id = NodeIdentity::load_or_create(path).expect("create keypair");
    println!("written {path:?}");
    println!("pubkey {}", hex(&id.pubkey()));
}

fn cmd_pubkey(args: &[String]) {
    if args.len() != 1 {
        eprintln!("usage: pubkey <key>");
        std::process::exit(2);
    }
    let id = NodeIdentity::load_or_create(&args[0]).expect("load keypair");
    println!("{}", hex(&id.pubkey()));
}

fn cmd_sign_whitelist(args: &[String]) {
    let mut admin_path: Option<String> = None;
    let mut out_path: Option<String> = None;
    let mut nodes: Vec<WhitelistEntry> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--admin" => {
                admin_path = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--out" => {
                out_path = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--node" => {
                let spec = args.get(i + 1).cloned().unwrap_or_default();
                match parse_node_spec(&spec) {
                    Some(e) => nodes.push(e),
                    None => {
                        eprintln!("bad --node {spec:?}: expected ADDR=PUBKEY_HEX:ZONE");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            other => {
                eprintln!("unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let admin_path = admin_path.unwrap_or_else(|| {
        eprintln!("--admin <key> is required");
        std::process::exit(2);
    });
    let out_path = out_path.unwrap_or_else(|| {
        eprintln!("--out <file> is required");
        std::process::exit(2);
    });
    if nodes.is_empty() {
        eprintln!("at least one --node is required");
        std::process::exit(2);
    }
    let admin = NodeIdentity::load_or_create(&admin_path).expect("admin key");
    let wl = Whitelist::sign(nodes, &admin);
    std::fs::write(&out_path, wl.encode()).expect("write whitelist");
    println!("wrote {out_path}");
    println!("admin pubkey: {}", hex(&wl.admin_pubkey));
    println!("{} nodes signed", wl.len());
}

fn cmd_verify_whitelist(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: verify-whitelist <file> [--admin-pubkey HEX]");
        std::process::exit(2);
    }
    let file = &args[0];
    let bytes = std::fs::read(file).expect("read whitelist");
    let wl = Whitelist::decode(&bytes).expect("decode whitelist");
    let mut expected: Option<[u8; PUBKEY_LEN]> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--admin-pubkey" {
            let hexstr = args.get(i + 1).cloned().unwrap_or_default();
            expected = Some(parse_pubkey_hex(&hexstr).expect("bad pubkey hex"));
            i += 2;
        } else {
            i += 1;
        }
    }
    if wl.verify(expected.as_ref()) {
        println!("ok");
    } else {
        eprintln!("BAD signature");
        std::process::exit(1);
    }
}

fn cmd_show_whitelist(args: &[String]) {
    if args.len() != 1 {
        eprintln!("usage: show-whitelist <file>");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&args[0]).expect("read whitelist");
    let wl = Whitelist::decode(&bytes).expect("decode whitelist");
    println!("admin pubkey: {}", hex(&wl.admin_pubkey));
    println!("entries: {}", wl.len());
    for e in &wl.entries {
        println!("  {:<22} zone={} pubkey={}", e.addr, e.zone, hex(&e.pubkey));
    }
    println!(
        "signature valid: {}",
        if wl.verify(None) { "yes" } else { "NO" }
    );
}

fn parse_node_spec(spec: &str) -> Option<WhitelistEntry> {
    // ADDR=PUBKEY_HEX:ZONE
    let eq = spec.find('=')?;
    let addr = spec[..eq].to_string();
    let rest = &spec[eq + 1..];
    let colon = rest.rfind(':')?;
    let pubkey_hex = &rest[..colon];
    let zone: u8 = rest[colon + 1..].parse().ok()?;
    let pubkey = parse_pubkey_hex(pubkey_hex)?;
    Some(WhitelistEntry { addr, pubkey, zone })
}

fn parse_pubkey_hex(s: &str) -> Option<[u8; PUBKEY_LEN]> {
    if s.len() != PUBKEY_LEN * 2 {
        return None;
    }
    let mut out = [0u8; PUBKEY_LEN];
    for i in 0..PUBKEY_LEN {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
