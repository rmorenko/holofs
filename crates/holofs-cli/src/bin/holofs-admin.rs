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
                                        (--force allows non-empty target; caller's problem)"
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
    let expected_admin: Option<[u8; PUBKEY_LEN]> = if args.len() >= 3 && args[1] == "--admin-pubkey" {
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
        die(format!(
            "GET {url}: HTTP {}",
            resp.status().as_u16()
        ));
    }
    let bytes = resp
        .bytes()
        .unwrap_or_else(|e| die(format!("read body: {e}")));
    match &f.output {
        Some(path) => {
            fs::write(path, &bytes).unwrap_or_else(|e| die(format!("write {}: {e}", path.display())));
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
    put_object(&gw, &name, body, f.content_type.as_deref(), f.admin_token.as_deref());
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
        die(format!(
            "PUT {url}: HTTP {} — {body}",
            status.as_u16()
        ));
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
    let out_dir = f.output_dir.unwrap_or_else(|| flag_required("--output-dir").into());
    fs::create_dir_all(out_dir.join("objects"))
        .unwrap_or_else(|e| die(format!("mkdir {}: {e}", out_dir.display())));
    let client = http_client();

    // 1. Enumerate catalog names.
    let list_url = format!("{}/admin/catalog_names", gw.trim_end_matches('/'));
    let listing: Vec<serde_json::Value> = client
        .get(&list_url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .unwrap_or_else(|e| die(format!("GET {list_url}: {e}")))
        .json()
        .unwrap_or_else(|e| die(format!("decode listing: {e}")));

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
            eprintln!(
                "  ! skipping {name}: HTTP {}",
                resp.status().as_u16()
            );
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
    let json = serde_json::to_vec_pretty(&idx)
        .unwrap_or_else(|e| die(format!("serialize index: {e}")));
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
    let in_dir = f.input_dir.unwrap_or_else(|| flag_required("--input-dir").into());
    let token = f.admin_token;
    let index_path = in_dir.join("index.json");
    let raw = fs::read(&index_path)
        .unwrap_or_else(|e| die(format!("read {}: {e}", index_path.display())));
    let idx: IndexFile = serde_json::from_slice(&raw)
        .unwrap_or_else(|e| die(format!("parse index: {e}")));
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
        let body = fs::read(&path)
            .unwrap_or_else(|e| die(format!("read {}: {e}", path.display())));
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
        return Err(format!("--storage {} is not a directory", storage.display()));
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
pub(crate) fn restore_tar_to_dir(
    input: &Path,
    storage: &Path,
    force: bool,
) -> Result<(), String> {
    if storage.exists() {
        let non_empty = fs::read_dir(storage).map(|it| it.count() > 0).unwrap_or(false);
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
    let in_file =
        fs::File::open(input).map_err(|e| format!("open {}: {e}", input.display()))?;
    let mut archive = tar::Archive::new(in_file);
    archive
        .unpack(storage)
        .map_err(|e| format!("unpack {}: {e}", storage.display()))?;
    Ok(())
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

        let size = snapshot_dir_to_tar(src.path(), tarball.path())
            .expect("snapshot should succeed");
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
        assert!(dst.path().join("existing.txt").exists(), "must not have unpacked");

        // With --force it goes through.
        restore_tar_to_dir(tarball.path(), dst.path(), true).unwrap();
        assert!(dst.path().join("a.txt").exists());
        assert!(dst.path().join("existing.txt").exists(), "pre-existing file kept");
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
