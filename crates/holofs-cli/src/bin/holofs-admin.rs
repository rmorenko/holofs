//! Admin utility: keys / whitelist / backup / snapshot.
//!
//! ```sh
//! # keys + whitelist (unchanged since 1.0)
//! holofs-admin gen-key admin.key
//! holofs-admin pubkey admin.key
//! holofs-admin sign-whitelist \\
//!   --admin admin.key \\
//!   --node 127.0.0.1:5000=PUBKEY_HEX:0 \\
//!   --node 127.0.0.1:5001=PUBKEY_HEX:1 \\
//!   --out cluster.wl
//!
//! # object-level backup (P0.2). Uses the gateway HTTP surface.
//! holofs-admin export     --gateway http://GW --name docs/note.txt --output note.bin
//! holofs-admin import     --gateway http://GW --name docs/note.txt --input note.bin
//! holofs-admin export-all --gateway http://GW --admin-token TOK --output-dir /backup/2026-07-17
//! holofs-admin import-all --gateway http://GW --input-dir /backup/2026-07-17
//!
//! # cluster-level snapshot (per-host, offline: pack the storage-dir).
//! holofs-admin snapshot         --storage /var/lib/holofs/node-0 --output node-0.tar
//! holofs-admin snapshot-restore --input node-0.tar --storage /var/lib/holofs/node-0
//! ```
//!
//! `--node` format: `ADDR=PUBKEY_HEX:ZONE`. PUBKEY_HEX is taken from the
//! output of `holofs-node --storage <dir>` at first start.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
        "export" => cmd_export(&args[1..]),
        "import" => cmd_import(&args[1..]),
        "export-all" => cmd_export_all(&args[1..]),
        "import-all" => cmd_import_all(&args[1..]),
        "snapshot" => cmd_snapshot(&args[1..]),
        "snapshot-restore" => cmd_snapshot_restore(&args[1..]),
        "drain-node" => cmd_drain_node(&args[1..]),
        "capacity" => cmd_capacity(&args[1..]),
        "set-retention" => cmd_set_retention(&args[1..]),
        "clear-retention" => cmd_clear_retention(&args[1..]),
        "show-retention" => cmd_show_retention(&args[1..]),
        "rotate-kek" => cmd_rotate_kek(&args[1..]),
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
keys + whitelist:\n\
  gen-key <out.key>                     generate a new Ed25519 keypair\n\
  pubkey <key>                          print pubkey hex\n\
  sign-whitelist --admin <key> --node ADDR=PUBKEY_HEX:ZONE [--node …] --out <file>\n\
                                        sign the cluster whitelist\n\
  verify-whitelist <file> [--admin-pubkey HEX]\n\
                                        verify the signature (opt. against expected admin)\n\
  show-whitelist <file>                 print contents\n\
\n\
backup / restore (P0.2 — object-level, HTTP-driven):\n\
  export     --gateway URL --name NAME [--output FILE]\n\
                                        fetch object body (default: stdout)\n\
  import     --gateway URL --name NAME [--input FILE] [--admin-token TOK] [--content-type MIME]\n\
                                        PUT object body (default: stdin)\n\
  export-all --gateway URL --admin-token TOK --output-dir DIR\n\
                                        enumerate catalog + dump every object to DIR/objects/*.bin\n\
                                        + write DIR/index.json (name → file map)\n\
  import-all --gateway URL --input-dir DIR [--admin-token TOK]\n\
                                        read DIR/index.json + PUT every listed object\n\
\n\
snapshot / restore (P0.2 — cluster-level, offline per host):\n\
  snapshot         --storage DIR --output FILE.tar\n\
                                        pack a node/gateway storage dir into an off-site tar\n\
  snapshot-restore --input FILE.tar --storage DIR [--force]\n\
                                        unpack a snapshot into a fresh storage dir\n\
                                        (--force allows non-empty target; caller's problem)\n\
\n\
cluster elasticity (P1.4 — drain + decommission):\n\
  drain-node --gateway URL --admin-token TOK --idx N [--purge]\n\
                                        flip admin_kills[N]=true + rebalance every catalog\n\
                                        entry so shards HRW-assigned to N land on live\n\
                                        neighbours. --purge additionally wipes shards from\n\
                                        the drained node after a successful sweep.\n\
                                        Follow-up: re-sign whitelist WITHOUT the drained\n\
                                        node + hot-reload; then physically stop the node.\n\
                                        See docs/operations.md §10.2.\n\
\n\
cluster capacity (P1.4b — observability + auto-rebalance signal):\n\
  capacity --gateway URL\n\
                                        print the per-node capacity table cached by the\n\
                                        gateway's capacity poller: idx, addr, used%,\n\
                                        free/total (GiB), live-bytes, freshness (secs since\n\
                                        last successful poll). Also shows cluster-wide skew\n\
                                        (min/max used-%, ratio). Auto-rebalance daemon\n\
                                        fires when skew crosses HOLOFS_REBALANCE_TRIGGER_PCT\n\
                                        (default 85) AND the coldest node is below\n\
                                        HOLOFS_REBALANCE_COLD_CEILING_PCT (default 60).\n\
                                        No admin-token needed — /api/capacity is read-only.\n\
\n\
per-object retention (P2.2 — GC daemon deletes on expiry):\n\
  set-retention   --gateway URL --admin-token TOK --name NAME --expires-at-unix N\n\
                                        set an absolute deletion deadline. `N` is Unix epoch\n\
                                        seconds. Setting in the past is legal — object becomes\n\
                                        eligible on the next GC tick. See\n\
                                        HOLOFS_RETENTION_GC_INTERVAL_SECS (default 3600).\n\
  clear-retention --gateway URL --admin-token TOK --name NAME\n\
                                        remove any retention policy from NAME.\n\
  show-retention  --gateway URL --admin-token TOK --name NAME\n\
                                        print the current policy as JSON. `null` = no policy.\n\
\n\
at-rest key rotation (P1.7 — envelope encryption, offline per node):\n\
  rotate-kek --storage DIR\n\
                                        append a fresh DEK to the node's keyring, wrap\n\
                                        under the same KEK as existing entries, bump\n\
                                        current_id. All new shard writes use the new DEK;\n\
                                        existing sealed shards remain readable via the\n\
                                        retained DEKs (try-each-key on decrypt, AES-GCM\n\
                                        tag selects). Reads HOLOFS_AT_REST_KEK_* env vars\n\
                                        exactly as `holofs-node` does — run this on the\n\
                                        node host with the same env, then restart the daemon\n\
                                        so it picks up the new keyring. See operations.md §5.9."
    );
}

// === keys + whitelist (unchanged from 1.0) =========================

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
        eprintln!("pubkey: expected exactly one argument (path to key)");
        std::process::exit(2);
    }
    match NodeIdentity::load_or_create(&args[0]) {
        Ok(id) => println!("{}", hex(&id.pubkey())),
        Err(e) => {
            eprintln!("load {}: {e}", args[0]);
            std::process::exit(1);
        }
    }
}

fn cmd_sign_whitelist(args: &[String]) {
    let mut admin_path: Option<String> = None;
    let mut out_path: Option<String> = None;
    let mut nodes: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--admin" => {
                admin_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--out" => {
                out_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--node" => {
                nodes.push(args[i + 1].clone());
                i += 2;
            }
            other => {
                eprintln!("unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let admin_path = admin_path.unwrap_or_else(|| {
        eprintln!("--admin is required");
        std::process::exit(2)
    });
    let out_path = out_path.unwrap_or_else(|| {
        eprintln!("--out is required");
        std::process::exit(2)
    });
    if nodes.is_empty() {
        eprintln!("at least one --node ADDR=PUBKEY_HEX:ZONE is required");
        std::process::exit(2);
    }
    let admin = match NodeIdentity::load_or_create(&admin_path) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("load admin key {admin_path}: {e}");
            std::process::exit(1);
        }
    };
    let mut entries: Vec<WhitelistEntry> = Vec::with_capacity(nodes.len());
    for spec in &nodes {
        match parse_node_spec(spec) {
            Some(e) => entries.push(e),
            None => {
                eprintln!("bad --node {spec:?}: expected ADDR=PUBKEY_HEX:ZONE");
                std::process::exit(2);
            }
        }
    }
    let wl = Whitelist::sign(entries, &admin);
    if let Err(e) = fs::write(&out_path, wl.encode()) {
        eprintln!("write {out_path}: {e}");
        std::process::exit(1);
    }
    println!(
        "wrote {out_path} ({} entries, admin pubkey {})",
        wl.len(),
        hex(&wl.admin_pubkey)
    );
}

fn cmd_verify_whitelist(args: &[String]) {
    if args.is_empty() {
        eprintln!("verify-whitelist: expected <file> [--admin-pubkey HEX]");
        std::process::exit(2);
    }
    let path = &args[0];
    let expected_admin: Option<[u8; PUBKEY_LEN]> = if args.len() >= 3 && args[1] == "--admin-pubkey"
    {
        parse_pubkey_hex(&args[2])
    } else {
        None
    };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
    };
    let wl = match Whitelist::decode(&bytes) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("decode: {e}");
            std::process::exit(1);
        }
    };
    let ok = wl.verify(expected_admin.as_ref());
    println!("signature valid: {}", if ok { "yes" } else { "NO" });
    if !ok {
        std::process::exit(1);
    }
}

fn cmd_show_whitelist(args: &[String]) {
    if args.len() != 1 {
        eprintln!("show-whitelist: expected exactly one argument (path)");
        std::process::exit(2);
    }
    let bytes = match fs::read(&args[0]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read: {e}");
            std::process::exit(1);
        }
    };
    let wl = match Whitelist::decode(&bytes) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("decode: {e}");
            std::process::exit(1);
        }
    };
    println!("admin_pubkey = {}", hex(&wl.admin_pubkey));
    for e in &wl.entries {
        println!(
            "  addr={:<24} zone={} pubkey={}",
            e.addr,
            e.zone,
            hex(&e.pubkey)
        );
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

// === object-level backup (P0.2) ====================================

/// Common flag surface for backup / restore subcommands.
#[derive(Default)]
struct HttpFlags {
    gateway: Option<String>,
    admin_token: Option<String>,
    name: Option<String>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    content_type: Option<String>,
    output_dir: Option<PathBuf>,
    input_dir: Option<PathBuf>,
}

fn parse_http_flags(args: &[String]) -> HttpFlags {
    let mut f = HttpFlags::default();
    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize, name: &str| -> String {
            if *i + 1 >= args.len() {
                eprintln!("{name}: expected a value");
                std::process::exit(2);
            }
            let v = args[*i + 1].clone();
            *i += 2;
            v
        };
        match args[i].as_str() {
            "--gateway" => f.gateway = Some(next(&mut i, "--gateway")),
            "--admin-token" => f.admin_token = Some(next(&mut i, "--admin-token")),
            "--name" => f.name = Some(next(&mut i, "--name")),
            "--input" => f.input = Some(PathBuf::from(next(&mut i, "--input"))),
            "--output" => f.output = Some(PathBuf::from(next(&mut i, "--output"))),
            "--content-type" => f.content_type = Some(next(&mut i, "--content-type")),
            "--output-dir" => f.output_dir = Some(PathBuf::from(next(&mut i, "--output-dir"))),
            "--input-dir" => f.input_dir = Some(PathBuf::from(next(&mut i, "--input-dir"))),
            other => {
                eprintln!("unexpected flag: {other}");
                std::process::exit(2);
            }
        }
    }
    f
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        // Backups can involve large objects; disable the default
        // 30 s timeout for the request-level path. The blocking client
        // still respects Ctrl-C on stdin between calls.
        .timeout(None)
        .build()
        .expect("build reqwest client")
}

fn cmd_export(args: &[String]) {
    let f = parse_http_flags(args);
    let gw = f.gateway.unwrap_or_else(|| flag_required("--gateway"));
    let name = f.name.unwrap_or_else(|| flag_required("--name"));
    let url = format!("{}/{}", gw.trim_end_matches('/'), name);
    let client = http_client();
    let resp = client
        .get(&url)
        .send()
        .unwrap_or_else(|e| die(format!("GET {url}: {e}")));
    if !resp.status().is_success() {
        die(format!("GET {url}: HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp
        .bytes()
        .unwrap_or_else(|e| die(format!("read body: {e}")));
    match &f.output {
        Some(path) => {
            fs::write(path, &bytes)
                .unwrap_or_else(|e| die(format!("write {}: {e}", path.display())));
            eprintln!("wrote {} bytes to {}", bytes.len(), path.display());
        }
        None => {
            std::io::stdout()
                .write_all(&bytes)
                .unwrap_or_else(|e| die(format!("stdout: {e}")));
        }
    }
}

fn cmd_import(args: &[String]) {
    let f = parse_http_flags(args);
    let gw = f.gateway.unwrap_or_else(|| flag_required("--gateway"));
    let name = f.name.unwrap_or_else(|| flag_required("--name"));
    let body = match &f.input {
        Some(p) => fs::read(p).unwrap_or_else(|e| die(format!("read {}: {e}", p.display()))),
        None => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .unwrap_or_else(|e| die(format!("stdin: {e}")));
            buf
        }
    };
    put_object(
        &gw,
        &name,
        body,
        f.content_type.as_deref(),
        f.admin_token.as_deref(),
    );
}

fn put_object(
    gateway: &str,
    name: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
    admin_token: Option<&str>,
) {
    let url = format!("{}/{}", gateway.trim_end_matches('/'), name);
    let client = http_client();
    let ct = content_type.unwrap_or("application/octet-stream");
    let mut req = client
        .put(&url)
        .header(reqwest::header::CONTENT_TYPE, ct)
        .body(body);
    if let Some(tok) = admin_token {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let resp = req
        .send()
        .unwrap_or_else(|e| die(format!("PUT {url}: {e}")));
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        die(format!("PUT {url}: HTTP {} — {body}", status.as_u16()));
    }
    eprintln!("PUT {name} → {}", status.as_u16());
}

/// Row of `index.json`, one per exported object.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    size: u64,
    file: String,
}

/// Top-level structure of `<dir>/index.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexFile {
    format_version: u32,
    gateway: String,
    exported_at_unix: u64,
    objects: Vec<IndexEntry>,
}

fn cmd_export_all(args: &[String]) {
    let f = parse_http_flags(args);
    let gw = f.gateway.unwrap_or_else(|| flag_required("--gateway"));
    let token = f
        .admin_token
        .unwrap_or_else(|| flag_required("--admin-token"));
    let out_dir = f
        .output_dir
        .unwrap_or_else(|| flag_required("--output-dir").into());
    fs::create_dir_all(out_dir.join("objects"))
        .unwrap_or_else(|e| die(format!("mkdir {}: {e}", out_dir.display())));
    let client = http_client();

    // 1. Enumerate catalog names. P2.1 — request the paginated
    //    variant so the response stays bounded even on a 100k-object
    //    catalog. `next_cursor=null` signals end of iteration; the
    //    initial call omits `cursor` to start from the beginning.
    let base_list_url = format!("{}/admin/catalog_names", gw.trim_end_matches('/'));
    let mut listing: Vec<serde_json::Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        // limit=1000 matches the gateway's default; being explicit
        // documents intent (this loop is paginating on purpose, not
        // by accident of a schema change).
        let mut url = format!("{base_list_url}?limit=1000");
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                url.push_str("&cursor=");
                url.push_str(c);
            }
        }
        let page: serde_json::Value = client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .unwrap_or_else(|e| die(format!("GET {url}: {e}")))
            .json()
            .unwrap_or_else(|e| die(format!("decode listing page: {e}")));
        let items = page
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| die("paginated response missing `items` array".into()));
        listing.extend(items);
        match page.get("next_cursor") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => {
                cursor = Some(s.clone());
            }
            _ => break,
        }
    }

    // 2. Fetch each object; write to objects/NNNNN.bin; record in
    //    index.json. Numbering is stable across runs against the same
    //    catalog because the source iteration is BTreeMap-ordered.
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(listing.len());
    for (idx, row) in listing.iter().enumerate() {
        let name = row
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| die("bad listing row: missing name".into()));
        let kind = row
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("opaque")
            .to_string();
        let size = row.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let file_name = format!("{:08}.bin", idx + 1);
        let file_path = out_dir.join("objects").join(&file_name);
        let get_url = format!("{}/{name}", gw.trim_end_matches('/'));
        let resp = client
            .get(&get_url)
            .send()
            .unwrap_or_else(|e| die(format!("GET {get_url}: {e}")));
        if !resp.status().is_success() {
            eprintln!("  ! skipping {name}: HTTP {}", resp.status().as_u16());
            continue;
        }
        let bytes = resp
            .bytes()
            .unwrap_or_else(|e| die(format!("read {name}: {e}")));
        fs::write(&file_path, &bytes)
            .unwrap_or_else(|e| die(format!("write {}: {e}", file_path.display())));
        eprintln!("  {name} → objects/{file_name} ({} bytes)", bytes.len());
        entries.push(IndexEntry {
            name: name.to_string(),
            kind,
            size,
            file: file_name,
        });
    }

    // 3. Write index.json.
    let idx = IndexFile {
        format_version: 1,
        gateway: gw.clone(),
        exported_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        objects: entries,
    };
    let index_path = out_dir.join("index.json");
    let json =
        serde_json::to_vec_pretty(&idx).unwrap_or_else(|e| die(format!("serialize index: {e}")));
    fs::write(&index_path, &json)
        .unwrap_or_else(|e| die(format!("write {}: {e}", index_path.display())));

    eprintln!(
        "\nexported {} object(s) to {} (index: {})",
        idx.objects.len(),
        out_dir.display(),
        index_path.display()
    );
}

fn cmd_import_all(args: &[String]) {
    let f = parse_http_flags(args);
    let gw = f.gateway.unwrap_or_else(|| flag_required("--gateway"));
    let in_dir = f
        .input_dir
        .unwrap_or_else(|| flag_required("--input-dir").into());
    let token = f.admin_token;
    let index_path = in_dir.join("index.json");
    let raw = fs::read(&index_path)
        .unwrap_or_else(|e| die(format!("read {}: {e}", index_path.display())));
    let idx: IndexFile =
        serde_json::from_slice(&raw).unwrap_or_else(|e| die(format!("parse index: {e}")));
    if idx.format_version != 1 {
        die(format!(
            "unsupported index format_version {}",
            idx.format_version
        ));
    }
    eprintln!(
        "importing {} object(s) from {} (exported from {})",
        idx.objects.len(),
        in_dir.display(),
        idx.gateway
    );
    for entry in &idx.objects {
        let path = in_dir.join("objects").join(&entry.file);
        let body = fs::read(&path).unwrap_or_else(|e| die(format!("read {}: {e}", path.display())));
        let content_type = kind_to_content_type(&entry.kind);
        put_object(&gw, &entry.name, body, Some(content_type), token.as_deref());
    }
    eprintln!("\nimported {} object(s)", idx.objects.len());
}

fn kind_to_content_type(kind: &str) -> &'static str {
    match kind {
        "image" => "image/png",
        "text" => "text/plain; charset=utf-8",
        "audio" => "audio/wav",
        _ => "application/octet-stream",
    }
}

// === cluster-level snapshot (P0.2) =================================

fn cmd_snapshot(args: &[String]) {
    let mut storage: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--storage" => {
                storage = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--output" => {
                output = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let storage = storage.unwrap_or_else(|| flag_required("--storage").into());
    let output = output.unwrap_or_else(|| flag_required("--output").into());
    match snapshot_dir_to_tar(&storage, &output) {
        Ok(size) => eprintln!(
            "snapshot {} → {} ({} bytes)",
            storage.display(),
            output.display(),
            size
        ),
        Err(e) => die(e),
    }
}

fn cmd_snapshot_restore(args: &[String]) {
    let mut input: Option<PathBuf> = None;
    let mut storage: Option<PathBuf> = None;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--input" => {
                input = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--storage" => {
                storage = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let input = input.unwrap_or_else(|| flag_required("--input").into());
    let storage = storage.unwrap_or_else(|| flag_required("--storage").into());
    if let Err(e) = restore_tar_to_dir(&input, &storage, force) {
        die(e);
    }
    eprintln!("restored {} → {}", input.display(), storage.display());
}

/// Pure helper: pack every file under `storage` into a tar at
/// `output`. Returns the on-disk size of the tar. Extracted from
/// `cmd_snapshot` so unit tests can exercise the round-trip without
/// argv parsing or `std::process::exit`.
pub(crate) fn snapshot_dir_to_tar(storage: &Path, output: &Path) -> Result<u64, String> {
    if !storage.is_dir() {
        return Err(format!(
            "--storage {} is not a directory",
            storage.display()
        ));
    }
    let out_file =
        fs::File::create(output).map_err(|e| format!("create {}: {e}", output.display()))?;
    let mut builder = tar::Builder::new(out_file);
    builder
        .append_dir_all(".", storage)
        .map_err(|e| format!("tar {}: {e}", storage.display()))?;
    builder
        .finish()
        .map_err(|e| format!("finish tar {}: {e}", output.display()))?;
    Ok(fs::metadata(output).map(|m| m.len()).unwrap_or(0))
}

/// Pure helper: unpack `input` into `storage`. `force` skips the
/// non-empty-directory refusal — callers who understand they're
/// overlaying an existing dir opt in explicitly.
pub(crate) fn restore_tar_to_dir(input: &Path, storage: &Path, force: bool) -> Result<(), String> {
    if storage.exists() {
        let non_empty = fs::read_dir(storage)
            .map(|it| it.count() > 0)
            .unwrap_or(false);
        if non_empty && !force {
            return Err(format!(
                "--storage {} is not empty; pass --force to overwrite \
                 (existing files inside will remain until tar overrides them)",
                storage.display()
            ));
        }
    } else {
        fs::create_dir_all(storage).map_err(|e| format!("mkdir {}: {e}", storage.display()))?;
    }
    let in_file = fs::File::open(input).map_err(|e| format!("open {}: {e}", input.display()))?;
    let mut archive = tar::Archive::new(in_file);
    archive
        .unpack(storage)
        .map_err(|e| format!("unpack {}: {e}", storage.display()))?;
    Ok(())
}

// === cluster elasticity (P1.4) ====================================

fn cmd_drain_node(args: &[String]) {
    let mut gateway: Option<String> = None;
    let mut admin_token: Option<String> = None;
    let mut idx: Option<usize> = None;
    let mut purge = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gateway" => {
                gateway = Some(args[i + 1].clone());
                i += 2;
            }
            "--admin-token" => {
                admin_token = Some(args[i + 1].clone());
                i += 2;
            }
            "--idx" => {
                idx = Some(args[i + 1].parse().unwrap_or_else(|e| {
                    die(format!("--idx: {e}"));
                }));
                i += 2;
            }
            "--purge" => {
                purge = true;
                i += 1;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let gateway = gateway.unwrap_or_else(|| flag_required("--gateway"));
    let admin_token = admin_token.unwrap_or_else(|| flag_required("--admin-token"));
    let idx = idx.unwrap_or_else(|| {
        eprintln!("--idx is required");
        std::process::exit(2);
    });

    let url = format!("{}/admin/drain_node", gateway.trim_end_matches('/'));
    let body = format!("idx={}&purge={}", idx, purge);
    let client = http_client();
    let resp = client
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {admin_token}"),
        )
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .unwrap_or_else(|e| die(format!("POST {url}: {e}")));
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        die(format!("POST {url}: HTTP {} — {text}", status.as_u16()));
    }
    println!("{text}");
}

// === cluster capacity (P1.4b) =====================================

fn cmd_capacity(args: &[String]) {
    let mut gateway: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gateway" => {
                gateway = Some(args[i + 1].clone());
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let gateway = gateway.unwrap_or_else(|| flag_required("--gateway"));

    let url = format!("{}/api/capacity", gateway.trim_end_matches('/'));
    let client = http_client();
    let resp = client
        .get(&url)
        .send()
        .unwrap_or_else(|e| die(format!("GET {url}: {e}")));
    let status = resp.status();
    let text = resp
        .text()
        .unwrap_or_else(|e| die(format!("GET {url}: read body: {e}")));
    if !status.is_success() {
        die(format!("GET {url}: HTTP {} — {text}", status.as_u16()));
    }
    let payload: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| die(format!("GET {url}: parse body: {e}")));

    // Table: idx | addr | used% | free/total GiB | live GiB | age
    // Widths chosen to keep the row under 100 cols on a typical
    // (address <= 50 chars) cluster; long addresses just wrap.
    println!(
        "{:>3}  {:<40}  {:>7}  {:>10}  {:>10}  {:>10}  {:>6}",
        "idx", "addr", "used%", "free/GiB", "total/GiB", "live/GiB", "age(s)"
    );
    println!(
        "{:-<3}  {:-<40}  {:->7}  {:->10}  {:->10}  {:->10}  {:->6}",
        "", "", "", "", "", "", ""
    );
    let nodes = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for node in &nodes {
        let idx = node.get("idx").and_then(|v| v.as_u64()).unwrap_or(0);
        let addr = node
            .get("addr")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let known = node
            .get("capacity_known")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let used_pct = node.get("used_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let free = node.get("free_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let total = node
            .get("total_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let live = node.get("live_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let age = node.get("age_secs").and_then(|v| v.as_u64());

        let used_str = if known {
            format!("{:>6.2}%", used_pct)
        } else {
            "  n/a".into()
        };
        let free_gib = if known {
            format!("{:.2}", free as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            "-".into()
        };
        let total_gib = if known {
            format!("{:.2}", total as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            "-".into()
        };
        let live_gib = format!("{:.2}", live as f64 / (1024.0 * 1024.0 * 1024.0));
        let age_str = age.map(|a| a.to_string()).unwrap_or_else(|| "-".into());
        println!(
            "{:>3}  {:<40}  {:>7}  {:>10}  {:>10}  {:>10}  {:>6}",
            idx, addr, used_str, free_gib, total_gib, live_gib, age_str
        );
    }

    // Skew footer.
    if let Some(cluster) = payload.get("cluster") {
        let known = cluster
            .get("known_nodes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let min = cluster.get("min_used_pct").and_then(|v| v.as_f64());
        let max = cluster.get("max_used_pct").and_then(|v| v.as_f64());
        let ratio = cluster.get("skew_ratio").and_then(|v| v.as_f64());
        println!();
        match (min, max, ratio) {
            (Some(mn), Some(mx), Some(r)) => println!(
                "cluster skew: known_nodes={known} min={:.2}% max={:.2}% ratio={:.2}x",
                mn, mx, r
            ),
            _ => println!("cluster skew: known_nodes={known} (need >=2 known nodes to compare)"),
        }
    }
}

// === per-object retention (P2.2) ==================================

fn cmd_set_retention(args: &[String]) {
    let mut gateway: Option<String> = None;
    let mut admin_token: Option<String> = None;
    let mut name: Option<String> = None;
    let mut expires_at_unix: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gateway" => {
                gateway = Some(args[i + 1].clone());
                i += 2;
            }
            "--admin-token" => {
                admin_token = Some(args[i + 1].clone());
                i += 2;
            }
            "--name" => {
                name = Some(args[i + 1].clone());
                i += 2;
            }
            "--expires-at-unix" => {
                expires_at_unix = Some(
                    args[i + 1]
                        .parse()
                        .unwrap_or_else(|e| die(format!("--expires-at-unix: {e}"))),
                );
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let gateway = gateway.unwrap_or_else(|| flag_required("--gateway"));
    let admin_token = admin_token.unwrap_or_else(|| flag_required("--admin-token"));
    let name = name.unwrap_or_else(|| flag_required("--name"));
    let expires_at_unix = expires_at_unix.unwrap_or_else(|| {
        eprintln!("--expires-at-unix is required (use `clear-retention` to remove)");
        std::process::exit(2);
    });

    let body = format!(
        "name={}&expires_at_unix={}",
        urlencoding_encode(&name),
        expires_at_unix,
    );
    let out = admin_post("/admin/retention", &gateway, &admin_token, body);
    println!("{out}");
}

fn cmd_clear_retention(args: &[String]) {
    let mut gateway: Option<String> = None;
    let mut admin_token: Option<String> = None;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gateway" => {
                gateway = Some(args[i + 1].clone());
                i += 2;
            }
            "--admin-token" => {
                admin_token = Some(args[i + 1].clone());
                i += 2;
            }
            "--name" => {
                name = Some(args[i + 1].clone());
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let gateway = gateway.unwrap_or_else(|| flag_required("--gateway"));
    let admin_token = admin_token.unwrap_or_else(|| flag_required("--admin-token"));
    let name = name.unwrap_or_else(|| flag_required("--name"));

    let body = format!("name={}&clear=true", urlencoding_encode(&name));
    let out = admin_post("/admin/retention", &gateway, &admin_token, body);
    println!("{out}");
}

fn cmd_show_retention(args: &[String]) {
    let mut gateway: Option<String> = None;
    let mut admin_token: Option<String> = None;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gateway" => {
                gateway = Some(args[i + 1].clone());
                i += 2;
            }
            "--admin-token" => {
                admin_token = Some(args[i + 1].clone());
                i += 2;
            }
            "--name" => {
                name = Some(args[i + 1].clone());
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let gateway = gateway.unwrap_or_else(|| flag_required("--gateway"));
    let admin_token = admin_token.unwrap_or_else(|| flag_required("--admin-token"));
    let name = name.unwrap_or_else(|| flag_required("--name"));

    let url = format!(
        "{}/admin/retention/{}",
        gateway.trim_end_matches('/'),
        name.trim_start_matches('/')
    );
    let client = http_client();
    let resp = client
        .get(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {admin_token}"),
        )
        .send()
        .unwrap_or_else(|e| die(format!("GET {url}: {e}")));
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        die(format!("GET {url}: HTTP {} — {text}", status.as_u16()));
    }
    println!("{text}");
}

/// Shared POST helper for the admin/retention endpoints. Sends
/// `body` as urlencoded, returns response body as String or dies on
/// non-2xx.
fn admin_post(path: &str, gateway: &str, token: &str, body: String) -> String {
    let url = format!("{}{path}", gateway.trim_end_matches('/'));
    let client = http_client();
    let resp = client
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .unwrap_or_else(|e| die(format!("POST {url}: {e}")));
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        die(format!("POST {url}: HTTP {} — {text}", status.as_u16()));
    }
    text
}

/// Minimal URL-encoder for form-body values. Escapes only the
/// small set of characters that would break a `application/x-www-
/// form-urlencoded` roundtrip; catalog names are validated at PUT
/// time so we don't need a full-fat RFC 3986 encoder.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// === at-rest key rotation (P1.7) ==================================

fn cmd_rotate_kek(args: &[String]) {
    let mut storage: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--storage" => {
                storage = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => die(format!("unexpected flag: {other}")),
        }
    }
    let storage = storage.unwrap_or_else(|| flag_required("--storage").into());

    // Same KEK-source resolution as `holofs-node` — pulls from
    // HOLOFS_AT_REST_KEK_* env; identity-seed path reads
    // `<storage>/identity.key`.
    let identity_path = storage.join("identity.key");
    let identity = holofs_storage::identity::NodeIdentity::load_or_create(&identity_path)
        .unwrap_or_else(|e| die(format!("load {}: {e}", identity_path.display())));
    let kek_source = match std::env::var("HOLOFS_AT_REST_KEK_SOURCE")
        .unwrap_or_else(|_| "identity".to_string())
        .as_str()
    {
        "file" => holofs_storage::crypto::KekSource::File(
            std::env::var("HOLOFS_AT_REST_KEK_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    die("HOLOFS_AT_REST_KEK_SOURCE=file requires HOLOFS_AT_REST_KEK_PATH".into())
                }),
        ),
        "env" => holofs_storage::crypto::KekSource::EnvHex(
            std::env::var("HOLOFS_AT_REST_KEK_HEX").unwrap_or_else(|_| {
                die("HOLOFS_AT_REST_KEK_SOURCE=env requires HOLOFS_AT_REST_KEK_HEX".into())
            }),
        ),
        _ => holofs_storage::crypto::KekSource::IdentitySeed,
    };
    let kek = kek_source
        .load(&identity.to_bytes())
        .unwrap_or_else(|e| die(format!("load KEK ({}): {e}", kek_source.label())));

    let keyring_path = std::env::var("HOLOFS_KEYRING_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| storage.join("keyring.json"));
    let bootstrap_dek = holofs_storage::crypto::derive_shard_key(&identity.to_bytes());
    let mut ring =
        holofs_storage::crypto::Keyring::load_or_bootstrap(&keyring_path, &kek, bootstrap_dek)
            .unwrap_or_else(|e| die(format!("open keyring {}: {e}", keyring_path.display())));

    let before = ring.current_id();
    let new_id = ring.rotate(&kek);
    ring.save(&keyring_path)
        .unwrap_or_else(|e| die(format!("save keyring {}: {e}", keyring_path.display())));

    println!(
        "rotated {} → new DEK id={} (previous current_id={}; {} DEK(s) retained for reads); \
         restart the holofs-node daemon so it picks up the new keyring",
        keyring_path.display(),
        new_id,
        before,
        ring.len(),
    );
}

// === helpers =======================================================

fn flag_required(name: &str) -> String {
    eprintln!("{name} is required");
    std::process::exit(2);
}

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_restore_roundtrip_preserves_files() {
        // Full snapshot → restore cycle through the pure helpers.
        // Proves `snapshot_dir_to_tar` + `restore_tar_to_dir`
        // compose losslessly for the shape a real
        // `<storage>/{catalog.redb,identity.key,shards/…}` would
        // have.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let tarball = tempfile::NamedTempFile::new().unwrap();

        // Simulate a real storage-dir: catalog file, key file,
        // nested shard file.
        fs::write(src.path().join("catalog.redb"), b"\x00\x01\x02\x03").unwrap();
        fs::write(src.path().join("identity.key"), b"secret-seed-bytes").unwrap();
        fs::create_dir_all(src.path().join("shards/aa")).unwrap();
        fs::write(
            src.path().join("shards/aa/00112233.shard"),
            b"shard-payload-42",
        )
        .unwrap();

        let size =
            snapshot_dir_to_tar(src.path(), tarball.path()).expect("snapshot should succeed");
        assert!(size > 0);

        // Empty target — restore should populate it.
        let dst_root = dst.path().join("restored");
        restore_tar_to_dir(tarball.path(), &dst_root, false)
            .expect("restore should succeed into empty target");

        assert_eq!(
            fs::read(dst_root.join("catalog.redb")).unwrap(),
            b"\x00\x01\x02\x03"
        );
        assert_eq!(
            fs::read(dst_root.join("identity.key")).unwrap(),
            b"secret-seed-bytes"
        );
        assert_eq!(
            fs::read(dst_root.join("shards/aa/00112233.shard")).unwrap(),
            b"shard-payload-42"
        );
    }

    #[test]
    fn restore_refuses_non_empty_target_without_force() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let tarball = tempfile::NamedTempFile::new().unwrap();
        fs::write(src.path().join("a.txt"), b"x").unwrap();
        snapshot_dir_to_tar(src.path(), tarball.path()).unwrap();

        // Non-empty target, no --force → refusal.
        fs::write(dst.path().join("existing.txt"), b"do not overwrite").unwrap();
        let err = restore_tar_to_dir(tarball.path(), dst.path(), false)
            .expect_err("should refuse non-empty target");
        assert!(err.contains("not empty"), "got: {err}");
        assert!(
            dst.path().join("existing.txt").exists(),
            "must not have unpacked"
        );

        // With --force it goes through.
        restore_tar_to_dir(tarball.path(), dst.path(), true).unwrap();
        assert!(dst.path().join("a.txt").exists());
        assert!(
            dst.path().join("existing.txt").exists(),
            "pre-existing file kept"
        );
    }

    #[test]
    fn snapshot_rejects_non_directory_storage() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let tarball = tempfile::NamedTempFile::new().unwrap();
        let err = snapshot_dir_to_tar(f.path(), tarball.path())
            .expect_err("snapshot of a file (not dir) must fail");
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn index_file_serde_roundtrip() {
        let idx = IndexFile {
            format_version: 1,
            gateway: "http://gw.local:8787".into(),
            exported_at_unix: 1_720_000_000,
            objects: vec![
                IndexEntry {
                    name: "docs/note.txt".into(),
                    kind: "text".into(),
                    size: 42,
                    file: "00000001.bin".into(),
                },
                IndexEntry {
                    name: "img/pic.png".into(),
                    kind: "image".into(),
                    size: 1024,
                    file: "00000002.bin".into(),
                },
            ],
        };
        let json = serde_json::to_vec_pretty(&idx).unwrap();
        let back: IndexFile = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.format_version, idx.format_version);
        assert_eq!(back.gateway, idx.gateway);
        assert_eq!(back.exported_at_unix, idx.exported_at_unix);
        assert_eq!(back.objects.len(), 2);
        assert_eq!(back.objects[0].name, "docs/note.txt");
        assert_eq!(back.objects[0].file, "00000001.bin");
        assert_eq!(back.objects[1].kind, "image");
        assert_eq!(back.objects[1].size, 1024);
    }
}
