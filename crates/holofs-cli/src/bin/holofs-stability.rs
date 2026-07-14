//! holofs-stability — SLO-gate stability test.
//!
//! Unlike `holofs-soak` (chaos-under-overload, expects errors, produces a
//! post-mortem report), this binary asserts a much stricter contract:
//! the cluster processes a bounded, prod-realistic workload for `--duration`
//! **without a single unexpected failure**. Any 5xx, timeout, SHA-256
//! mismatch, panic-in-log, or dangling catalog entry aborts the run
//! immediately.
//!
//! Workload shape:
//! - **Phase 1 (ramp-fill)** — fill the cluster to ~0.9 × `--target-size`
//!   with random binaries drawn from `--file-size-mix`. Each PUT is
//!   immediately GET-verified against its SHA-256.
//! - **Phase 2 (steady churn)** — hold the size within ±10% of target
//!   for the remaining budget. Bang-bang controller: below 0.85× → force
//!   `put_new`; above 0.95× → force `delete`; otherwise pick a weighted
//!   op from the full menu (get, range_get, mv, mkdir, rmdir, versions,
//!   stats, health, gc — plus search/similar/spotlight if `--enable-embed`).
//! - **Phase 3 (drain)** — delete everything the test created, then
//!   assert `/api/stats.objects_total == baseline`.
//!
//! The concurrency is **fixed at `floor(0.8 × --encode)`** (default 6 for
//! `encode=8`) — that puts the workload one step below the encoder
//! semaphore's admission point, so 503-backpressure never fires in a
//! healthy cluster. If you see it, either the cluster is unhealthy or
//! your `--encode` override made the target unreachable.
//!
//! Race-4xx (concurrent-delete → 404, rmdir-nonempty → 409, mv-into-just-
//! removed-dir → 400) are counted but tolerated. Everything else aborts.

#![allow(
    clippy::uninlined_format_args,
    clippy::items_after_statements,
    clippy::unreadable_literal,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    missing_docs
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use holofs_core::hash::sha256;

// ============================================================================
// CLI
// ============================================================================

/// Prod-realistic stability test — cluster must survive `--duration` with
/// zero unexpected failures under bounded steady-state churn.
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
struct Cli {
    // ---- run shape --------------------------------------------------------
    /// Total wall-clock duration. Accepts `24h`, `30m`, `1h30m`, `90s`.
    #[arg(long, default_value = "24h")]
    duration: String,
    /// Target steady-state footprint (all sizes with `k`/`M`/`G`/`Ki`/`Mi`/`Gi`).
    #[arg(long, default_value = "10GiB")]
    target_size: String,
    /// Mix of PUT payload sizes, `<size>:<weight>` pairs, weights sum >0.
    /// Weights are relative — `4k:2,1m:1` is 2× as many 4 KiB PUTs as 1 MiB.
    #[arg(long, default_value = "4k:20,64k:40,256k:25,1m:10,4m:5")]
    file_size_mix: String,
    /// Override for the RNG seed (deterministic reproduction). Omit → OS.
    #[arg(long)]
    seed: Option<u64>,

    // ---- gateway concurrency caps (prod defaults) -------------------------
    /// `HOLOFS_MEDIUM_CONCURRENCY` — GET / range / dirops semaphore.
    #[arg(long, default_value = "64")]
    medium: usize,
    /// `HOLOFS_LONG_CONCURRENCY` — search / similar / spotlight semaphore.
    #[arg(long, default_value = "24")]
    long: usize,
    /// `HOLOFS_ENCODE_CONCURRENCY` — encoder semaphore (PUT admission).
    /// The test's worker count is derived from this: `floor(0.8 × encode)`.
    #[arg(long, default_value = "8")]
    encode: usize,
    /// `HOLOFS_ENCODE_QUEUE_MAX` — waiting-list cap before 202-fast-path 429.
    #[arg(long, default_value = "32")]
    encode_queue: usize,

    // ---- feature toggles --------------------------------------------------
    /// Enable per-object version history (`--enable-versions` on gateway).
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    enable_versions: bool,
    /// Cap retention (`HOLOFS_VERSIONS_KEEP_LAST`). 0 → unlimited.
    #[arg(long, default_value = "8")]
    versions_keep_last: u32,
    /// Enable CLIP semantic-search index (`--enable-embed` on gateway).
    /// Adds search/similar/spotlight ops to the mix.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    enable_embed: bool,
    /// At-rest shard encryption (`HOLOFS_AT_REST_ENC=1`).
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    at_rest_encryption: bool,
    /// Node fsync-on-write (`HOLOFS_NODE_FSYNC=1`).
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    fsync: bool,

    // ---- background loops -------------------------------------------------
    #[arg(long, default_value = "10m")]
    scrub_interval: String,
    #[arg(long, default_value = "30s")]
    audit_interval: String,
    #[arg(long, default_value = "5s")]
    monitor_interval: String,
    #[arg(long, default_value = "30s")]
    reputation_persist: String,

    // ---- transport --------------------------------------------------------
    /// Per-request HTTP timeout — used verbatim as the abort threshold.
    #[arg(long, default_value = "60s")]
    request_timeout: String,
    /// Gateway HTTP port.
    #[arg(long, default_value = "8822")]
    gateway_port: u16,
    /// Base port for embedded nodes (`HOLOFS_EMBED_BASE_PORT`).
    #[arg(long, default_value = "5500")]
    embed_base_port: u16,

    // ---- topology / binaries ---------------------------------------------
    /// Directory holding `holofs-web`. Defaults to the directory this
    /// `holofs-stability` binary was launched from.
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    /// Boot deadline for the spawned gateway.
    #[arg(long, default_value = "90s")]
    boot_timeout: String,

    /// Optional admin bearer token. When set, plumbed into the spawned
    /// gateway (`HOLOFS_ADMIN_TOKEN`) and sent on `/api/gc` and other
    /// admin routes. Unset → admin ops are dropped from the mix (the
    /// admin surface returns 403 with no token).
    #[arg(long)]
    admin_token: Option<String>,

    // ---- output -----------------------------------------------------------
    /// Artefact root. `<out>/<utc>/` is created inside.
    #[arg(long, default_value = ".stability")]
    out: PathBuf,
}

// ============================================================================
// Parsers
// ============================================================================

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let mut total_ms: u64 = 0;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: u64 = num.parse().with_context(|| format!("bad number in {s:?}"))?;
        num.clear();
        let ms = match ch {
            'd' | 'D' => n.checked_mul(86_400_000),
            'h' | 'H' => n.checked_mul(3_600_000),
            'm' | 'M' => n.checked_mul(60_000),
            's' | 'S' => n.checked_mul(1_000),
            other => bail!("unknown duration unit {other:?} in {s:?}"),
        }
        .ok_or_else(|| anyhow::anyhow!("duration overflow"))?;
        total_ms = total_ms.checked_add(ms).ok_or_else(|| anyhow::anyhow!("overflow"))?;
    }
    if !num.is_empty() {
        let n: u64 = num.parse()?;
        total_ms = total_ms.saturating_add(n.saturating_mul(1000));
    }
    Ok(Duration::from_millis(total_ms))
}

/// Parse `10GiB`, `256k`, `4m` etc. Accepts `k`, `M`, `G` (1000-based) and
/// `Ki`, `Mi`, `Gi` (1024-based). Case-insensitive suffix.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, suffix) = split_number_suffix(s);
    let n: u64 = num.parse().with_context(|| format!("bad size {s:?}"))?;
    let mult: u64 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1_000,
        "ki" => 1024,
        "m" => 1_000_000,
        "mi" => 1024 * 1024,
        "g" => 1_000_000_000,
        "gi" => 1024 * 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "mib" => 1024 * 1024,
        "kib" => 1024,
        other => bail!("unknown size suffix {other:?} in {s:?}"),
    };
    n.checked_mul(mult).ok_or_else(|| anyhow::anyhow!("size overflow in {s:?}"))
}

fn split_number_suffix(s: &str) -> (&str, &str) {
    let split_at = s.chars().take_while(|c| c.is_ascii_digit()).count();
    s.split_at(split_at)
}

#[derive(Debug, Clone)]
struct SizeBucket {
    bytes: u64,
    weight: u32,
}

fn parse_size_mix(s: &str) -> Result<Vec<SizeBucket>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let (sz, w) = part.split_once(':').with_context(|| format!("bad mix entry {part:?}"))?;
        out.push(SizeBucket {
            bytes: parse_size(sz)?,
            weight: w.parse().with_context(|| format!("bad weight in {part:?}"))?,
        });
    }
    if out.iter().map(|b| b.weight).sum::<u32>() == 0 {
        bail!("--file-size-mix weights sum to 0");
    }
    Ok(out)
}

fn pick_size(rng: &mut SmallRng, mix: &[SizeBucket]) -> u64 {
    let total: u32 = mix.iter().map(|b| b.weight).sum();
    let mut roll = rng.gen_range(0..total);
    for b in mix {
        if roll < b.weight {
            return b.bytes;
        }
        roll -= b.weight;
    }
    mix.last().unwrap().bytes
}

// ============================================================================
// Cluster spawn (embedded-only for v1)
// ============================================================================

struct Gateway {
    child: Child,
    storage_root: PathBuf,
    base_url: String,
    gateway_log: PathBuf,
}

impl Gateway {
    async fn shutdown(mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.start_kill();
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
    }
}

fn resolve_binary(dir: Option<&Path>, name: &str) -> PathBuf {
    let default_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let base = dir.map(Path::to_path_buf).or(default_dir).unwrap_or_default();
    let cand = base.join(name);
    if cand.exists() {
        return cand;
    }
    PathBuf::from(name)
}

async fn spawn_gateway(cli: &Cli, run_dir: &Path) -> Result<Gateway> {
    let bin_dir = cli.binary_dir.as_deref();
    let holofs_web = resolve_binary(bin_dir, "holofs-web");
    if !holofs_web.exists() {
        bail!("holofs-web binary not found at {} — pass --binary-dir?", holofs_web.display());
    }
    let storage_root = run_dir.join("cluster");
    std::fs::create_dir_all(&storage_root)?;
    let addr = format!("127.0.0.1:{}", cli.gateway_port);
    let base_url = format!("http://{addr}");
    let gateway_log = run_dir.join("gateway.log");
    let gw_stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gateway_log)
        .with_context(|| format!("open {}", gateway_log.display()))?;

    let mut cmd = Command::new(&holofs_web);
    cmd.arg("--addr").arg(&addr).arg("--storage").arg(&storage_root);
    if cli.enable_embed {
        cmd.arg("--enable-embed");
    }
    if cli.enable_versions {
        cmd.arg("--enable-versions");
    }
    // Every prod-relevant knob plumbed as env — matches how holofs-web
    // reads its own RuntimeConfig snapshot.
    cmd.env("HOLOFS_NO_SEED", "true")
        .env("HOLOFS_LOG", "warn")
        .env("HOLOFS_LOG_FORMAT", "text")
        .env("HOLOFS_EMBED_BASE_PORT", cli.embed_base_port.to_string())
        .env("HOLOFS_MEDIUM_CONCURRENCY", cli.medium.to_string())
        .env("HOLOFS_LONG_CONCURRENCY", cli.long.to_string())
        .env("HOLOFS_ENCODE_CONCURRENCY", cli.encode.to_string())
        .env("HOLOFS_ENCODE_QUEUE_MAX", cli.encode_queue.to_string())
        .env("HOLOFS_AT_REST_ENC", if cli.at_rest_encryption { "1" } else { "0" })
        .env("HOLOFS_NODE_FSYNC", if cli.fsync { "1" } else { "0" })
        .env("HOLOFS_MONITOR_INTERVAL", parse_duration(&cli.monitor_interval)?.as_secs().to_string())
        .env("HOLOFS_AUDIT_INTERVAL", parse_duration(&cli.audit_interval)?.as_secs().to_string())
        .env("HOLOFS_SCRUB_INTERVAL", parse_duration(&cli.scrub_interval)?.as_secs().to_string())
        .env(
            "HOLOFS_REPUTATION_PERSIST_INTERVAL",
            parse_duration(&cli.reputation_persist)?.as_secs().to_string(),
        );
    if cli.enable_versions && cli.versions_keep_last > 0 {
        cmd.env("HOLOFS_VERSIONS_KEEP_LAST", cli.versions_keep_last.to_string());
    }
    if let Some(tok) = cli.admin_token.as_deref() {
        cmd.env("HOLOFS_ADMIN_TOKEN", tok);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::from(gw_stderr)).kill_on_drop(true);

    eprintln!("[stability] spawning gateway {} → {}", holofs_web.display(), base_url);
    eprintln!("[stability] gateway stderr → {}", gateway_log.display());
    let child = cmd.spawn().with_context(|| format!("spawn {}", holofs_web.display()))?;
    let gw = Gateway { child, storage_root, base_url: base_url.clone(), gateway_log };
    wait_for_gateway(&base_url, parse_duration(&cli.boot_timeout)?).await?;
    eprintln!("[stability] gateway ready");
    Ok(gw)
}

async fn wait_for_gateway(base_url: &str, deadline: Duration) -> Result<()> {
    let start = Instant::now();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    let url = format!("{base_url}/api/stats");
    loop {
        if let Ok(r) = client.get(&url).send().await {
            if r.status().is_success() {
                return Ok(());
            }
        }
        if start.elapsed() > deadline {
            bail!("gateway did not answer within {deadline:?}");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

// ============================================================================
// Inventory + counters
// ============================================================================

#[derive(Debug, Clone)]
struct OwnedObject {
    name: String,
    size: u64,
    sha256_hex: String,
    kind: ObjKind,
    /// Verbatim `IngestResult` JSON returned by the PUT that produced
    /// this object. Preserved so a SHA-256 mismatch abort can print
    /// the server-reported `data_cid` / `object_id` — enough to tell
    /// "server stored the wrong object" from "codec round-trip loss".
    put_response: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjKind {
    Binary,
    Image,
}

#[derive(Default)]
struct Counters {
    put_new: AtomicU64,
    put_replace: AtomicU64,
    get: AtomicU64,
    range_get: AtomicU64,
    delete: AtomicU64,
    mkdir: AtomicU64,
    rmdir: AtomicU64,
    mv: AtomicU64,
    versions_list: AtomicU64,
    restore_version: AtomicU64,
    search: AtomicU64,
    similar: AtomicU64,
    spotlight: AtomicU64,
    stats: AtomicU64,
    health: AtomicU64,
    gc_orphans: AtomicU64,
    race_404: AtomicU64,
    race_409: AtomicU64,
    race_400: AtomicU64,
    bytes_put: AtomicU64,
    bytes_get: AtomicU64,
}

struct State {
    /// Everything the test PUT (and hasn't yet DELETEd). Name → metadata.
    owned: RwLock<HashMap<String, OwnedObject>>,
    /// Names currently being mutated or verified by a worker. Every op
    /// that snapshots a name from `owned` before making an HTTP call
    /// atomically moves it here first, so no second worker can pick
    /// the same name and race the underlying catalog entry. Restored
    /// (or dropped, on delete/mv success) when the op completes.
    ///
    /// Without this, two workers picking the same name for
    /// `put_replace` would race on `cat.insert(name, manifest)` inside
    /// the gateway: the winner's manifest lands in the catalog and the
    /// loser's post-PUT `verify_get` decodes the winner's bytes,
    /// producing a spurious SHA-256 mismatch abort.
    in_flight: RwLock<std::collections::HashSet<String>>,
    /// Fixed image pool for search/similar/spotlight. Never mutated after setup.
    image_names: Vec<String>,
    /// Subdirs the test created (and hasn't yet rmdir'd).
    dirs: RwLock<Vec<String>>,
    /// Bytes tracked as "our data" — put/delete increment/decrement.
    tracked_bytes: AtomicU64,
    /// objects_total from `/api/stats` before the test started.
    baseline_objects: u64,
    target_size: u64,
    counters: Counters,
    abort: CancellationToken,
    /// First reason a worker set `abort`. Populated once, read at shutdown.
    /// std::sync::Mutex on purpose — set_abort is called from sync
    /// contexts (worker error paths) and never contended long enough
    /// to justify the async variant.
    abort_reason: std::sync::Mutex<Option<String>>,
    /// Global sequence for object names (worker-agnostic — avoids
    /// racing worker-local counters).
    put_seq: AtomicU64,
    /// Test root under which all mutations live (`stability/<uuid>`).
    root: String,
    /// Base URL of the gateway.
    base: String,
    /// Reqwest client shared by every worker.
    client: reqwest::Client,
    /// Admin bearer token, empty when unset — sent on `/api/gc` etc.
    admin_token: String,
}

impl State {
    fn set_abort(&self, reason: String) {
        // First-writer wins; the token itself is set unconditionally so
        // subsequent workers still stop, but the reason logs the primary.
        let already = ABORT_SET.swap(true, Ordering::SeqCst);
        if !already {
            // best-effort: block briefly to store the reason.
            if let Ok(mut guard) = self.abort_reason.lock() {
                *guard = Some(reason.clone());
            }
            eprintln!("[stability] ABORT: {reason}");
        }
        self.abort.cancel();
    }
}

static ABORT_SET: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Race classifier
// ============================================================================

#[derive(Debug)]
enum Outcome {
    Ok,
    RaceExpected(&'static str, u16),
    Abort(String),
}

fn classify_status(op: &'static str, status: u16, body_hint: &str) -> Outcome {
    if (200..300).contains(&status) {
        return Outcome::Ok;
    }
    // Timeouts / transport errors bypass this and are handled by the caller.
    if status >= 500 {
        return Outcome::Abort(format!("{op}: unexpected {status} ({body_hint})"));
    }
    // 4xx whitelist by op — everything else is a real bug.
    let ok = match (op, status) {
        ("get" | "range_get" | "delete" | "mv" | "restore_version" | "versions_list", 404) => true,
        ("rmdir", 409) => true,
        ("mkdir", 409) => true, // parent race — dir already exists
        ("mv", 409) => true,
        ("put_new" | "put_replace", 400) => true, // parent removed mid-flight
        ("spotlight" | "search" | "similar", 404) => true,
        _ => false,
    };
    if ok {
        Outcome::RaceExpected(op, status)
    } else {
        Outcome::Abort(format!("{op}: unexpected {status} ({body_hint})"))
    }
}

// ============================================================================
// HTTP helpers
// ============================================================================

async fn http_get_bytes(client: &reqwest::Client, url: &str) -> Result<(u16, Vec<u8>)> {
    let resp = client.get(url).send().await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?.to_vec();
    Ok((status, body))
}

async fn http_get_text(client: &reqwest::Client, url: &str) -> Result<(u16, String)> {
    let resp = client.get(url).send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

// ============================================================================
// Payload synthesis
// ============================================================================

/// Header that forces the gateway's `put_any` auto-detect down the
/// opaque path: `0xFE` is invalid as a UTF-8 lead byte (so text kind
/// is rejected), and the ASCII tag that follows matches no image /
/// audio magic recognised by `image` or `symphonia`.
const OPAQUE_MARKER: &[u8] = b"\xFEHOLOFS-STABILITY\x00";

/// Random binary blob guaranteed to fall through `put_any`'s auto-
/// detect to the opaque path. Two invariants beyond the marker:
///
/// - No `0xFF` byte followed by a byte with the top 3 bits set — that
///   pattern is an MP3 sync frame, and symphonia's format probe
///   happily latches onto one anywhere in the buffer and stores the
///   object as lossy audio.
/// - No `0x52 0x49 0x46 0x46` (`RIFF`) prefix, ruled out by the
///   `OPAQUE_MARKER` at offset 0.
///
/// The masking cost is one branch per byte on top of the RNG fill;
/// negligible next to the network round-trip.
fn make_binary(rng: &mut SmallRng, size: u64) -> Vec<u8> {
    let size = (size as usize).max(OPAQUE_MARKER.len());
    let mut buf = vec![0u8; size];
    buf[..OPAQUE_MARKER.len()].copy_from_slice(OPAQUE_MARKER);
    rng.fill(&mut buf[OPAQUE_MARKER.len()..]);
    // Kill every MP3 sync pattern in one pass. An MP3 sync frame is
    // `0xFF` followed by a byte with `(b & 0xE0) == 0xE0` — mask those
    // top bits off the following byte whenever a `0xFF` appears.
    for i in OPAQUE_MARKER.len()..buf.len().saturating_sub(1) {
        if buf[i] == 0xFF {
            buf[i + 1] &= 0x1F;
        }
    }
    buf
}

/// Small synthetic PNG (32×32 gradient with an RNG-tinted hue) for the
/// search/similar/spotlight pool. Deterministic per `seed_tag` so a
/// re-run reproduces the same corpus.
fn make_image(seed_tag: u64) -> Vec<u8> {
    let w: u32 = 64;
    let h: u32 = 64;
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    let tint = (seed_tag & 0xff) as u8;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = ((x * 4) & 0xff) as u8 ^ tint;
            rgb[i + 1] = ((y * 4) & 0xff) as u8 ^ tint.rotate_left(3);
            rgb[i + 2] = ((x ^ y) as u8).wrapping_mul(3) ^ tint.rotate_left(5);
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("PNG header");
        writer.write_image_data(&rgb).expect("PNG data");
    }
    buf
}

// ============================================================================
// Workload primitives
// ============================================================================

/// Returns `(Outcome, response_body)`. On Ok / RaceExpected the body is
/// the server's JSON — the callers use it to enrich abort messages.
async fn put_object(
    state: &State,
    name: &str,
    body: Vec<u8>,
    content_type: &str,
) -> Result<(Outcome, String)> {
    let url = format!("{}/{}", state.base, name);
    let resp = state
        .client
        .put(&url)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let body_str = r.text().await.unwrap_or_default();
            let hint: String = body_str.chars().take(120).collect();
            Ok((classify_status("put_new", status, &hint), body_str))
        }
        Err(e) if e.is_timeout() => Ok((Outcome::Abort(format!("put {name}: timeout ({e})")), String::new())),
        Err(e) => Ok((Outcome::Abort(format!("put {name}: transport ({e})")), String::new())),
    }
}

async fn verify_get(state: &State, obj: &OwnedObject) -> Result<Outcome> {
    let url = format!("{}/{}", state.base, obj.name);
    match http_get_bytes(&state.client, &url).await {
        Ok((200, body)) => {
            state.counters.bytes_get.fetch_add(body.len() as u64, Ordering::Relaxed);
            let got = hex_of(&sha256(&body));
            if got != obj.sha256_hex {
                let head = body.iter().take(16).map(|b| format!("{b:02x}")).collect::<String>();
                Ok(Outcome::Abort(format!(
                    "verify {}: SHA256 mismatch (got {got} len={}, expected {} len={}, first16={head}, put_response={})",
                    obj.name,
                    body.len(),
                    obj.sha256_hex,
                    obj.size,
                    obj.put_response,
                )))
            } else {
                Ok(Outcome::Ok)
            }
        }
        Ok((status, body)) => Ok(classify_status("get", status, body_hint(&body).as_str())),
        Err(e) => Ok(Outcome::Abort(format!("verify {}: transport ({e})", obj.name))),
    }
}

async fn delete_object(state: &State, name: &str) -> Result<Outcome> {
    let url = format!("{}/{}", state.base, name);
    match state.client.delete(&url).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            Ok(classify_status("delete", status, body.chars().take(120).collect::<String>().as_str()))
        }
        Err(e) => Ok(Outcome::Abort(format!("delete {name}: transport ({e})"))),
    }
}

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn body_hint(body: &[u8]) -> String {
    std::str::from_utf8(body).unwrap_or("<binary>").chars().take(120).collect()
}

// ============================================================================
// Phase 1: ramp fill
// ============================================================================

async fn ramp_fill(state: Arc<State>, cli: &Cli, workers: usize) -> Result<()> {
    let mix = parse_size_mix(&cli.file_size_mix)?;
    let ceiling = (state.target_size as f64 * 0.90) as u64;
    eprintln!(
        "[stability] phase 1: ramp-fill to {} MiB with {} workers",
        ceiling / 1024 / 1024,
        workers
    );
    let mut handles = Vec::with_capacity(workers);
    for w in 0..workers {
        let state = Arc::clone(&state);
        let mix = mix.clone();
        let seed = cli.seed.unwrap_or_else(rand::random).wrapping_add(w as u64);
        handles.push(tokio::spawn(async move {
            let mut rng = SmallRng::seed_from_u64(seed);
            loop {
                if state.abort.is_cancelled() {
                    return;
                }
                if state.tracked_bytes.load(Ordering::Relaxed) >= ceiling {
                    return;
                }
                let size = pick_size(&mut rng, &mix);
                let seq = state.put_seq.fetch_add(1, Ordering::Relaxed);
                let name = format!("{}/w{:02}-{:07}.bin", state.root, w, seq);
                let body = make_binary(&mut rng, size);
                let sha = hex_of(&sha256(&body));
                let (outcome, put_resp) = match put_object(&state, &name, body, "application/octet-stream").await {
                    Ok(v) => v,
                    Err(e) => {
                        state.set_abort(format!("put {name}: {e}"));
                        return;
                    }
                };
                let obj = OwnedObject {
                    name: name.clone(),
                    size,
                    sha256_hex: sha,
                    kind: ObjKind::Binary,
                    put_response: put_resp,
                };
                match outcome {
                    Outcome::Ok => {
                        state.counters.put_new.fetch_add(1, Ordering::Relaxed);
                        state.counters.bytes_put.fetch_add(size, Ordering::Relaxed);
                        state.tracked_bytes.fetch_add(size, Ordering::Relaxed);
                        // Immediately verify — the whole reason ramp-fill exists is
                        // to prove PUT→GET→SHA256 round-trips under a growing catalog.
                        match verify_get(&state, &obj).await {
                            Ok(Outcome::Ok) => {}
                            Ok(Outcome::RaceExpected(_, _)) => {
                                // 404 immediately after successful PUT is not a race —
                                // no other worker has this name. Treat as abort.
                                state.set_abort(format!("verify {}: 404 immediately after PUT-2xx", obj.name));
                                return;
                            }
                            Ok(Outcome::Abort(msg)) => {
                                state.set_abort(msg);
                                return;
                            }
                            Err(e) => {
                                state.set_abort(format!("verify {}: {e}", obj.name));
                                return;
                            }
                        }
                        state.owned.write().await.insert(name, obj);
                        state.counters.get.fetch_add(1, Ordering::Relaxed);
                    }
                    Outcome::RaceExpected(_, s) => {
                        // Ramp-fill has no legitimate races — no concurrent deleters yet.
                        state.set_abort(format!("put {name}: unexpected {s} during ramp-fill"));
                        return;
                    }
                    Outcome::Abort(msg) => {
                        state.set_abort(msg);
                        return;
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    if state.abort.is_cancelled() {
        bail!("ramp-fill aborted");
    }
    eprintln!(
        "[stability] phase 1 done: {} objects, {} MiB tracked",
        state.owned.read().await.len(),
        state.tracked_bytes.load(Ordering::Relaxed) / 1024 / 1024
    );
    Ok(())
}

// ============================================================================
// Phase 2: steady churn
// ============================================================================

#[derive(Clone, Copy)]
#[allow(clippy::enum_variant_names)]
enum Op {
    PutNew,
    PutReplace,
    Get,
    RangeGet,
    Delete,
    Mkdir,
    Rmdir,
    Mv,
    VersionsList,
    RestoreVersion,
    Search,
    Similar,
    Spotlight,
    Stats,
    Health,
    GcOrphans,
}

fn base_weights(enable_embed: bool, enable_versions: bool, admin: bool) -> Vec<(Op, u32)> {
    let mut v = vec![
        (Op::Get, 30),
        (Op::PutNew, 10),
        (Op::PutReplace, 10),
        (Op::Delete, 10),
        (Op::RangeGet, 5),
        (Op::Mv, 5),
        (Op::Mkdir, 3),
        (Op::Rmdir, 3),
        (Op::Stats, 3),
        (Op::Health, 3),
    ];
    // Versions and similar ops today live on leptos server_fn URLs
    // (POST + serde-JSON body), which don't map cleanly onto the plain
    // reqwest form we use everywhere else. Version history is still
    // exercised transitively — every `put_replace` archives the prior
    // manifest when `--enable-versions` is on. `enable_versions` is
    // passed for future opt-in when a REST-flavour endpoint lands.
    let _ = enable_versions;
    if enable_embed {
        v.push((Op::Search, 5));
        v.push((Op::Spotlight, 3));
    }
    // `/api/gc` sits behind `AdminAuth`, so it 403s without a bearer
    // token. Include it only when the operator plumbed one — otherwise
    // the mix would abort on the first roll of GcOrphans.
    if admin {
        v.push((Op::GcOrphans, 2));
    }
    v
}

fn pick_op(rng: &mut SmallRng, weights: &[(Op, u32)]) -> Op {
    let total: u32 = weights.iter().map(|(_, w)| w).sum();
    let mut roll = rng.gen_range(0..total);
    for (op, w) in weights {
        if roll < *w {
            return *op;
        }
        roll -= w;
    }
    weights.last().unwrap().0
}

/// Bang-bang controller: below 0.85× → force PUT; above 0.95× → force
/// DELETE; otherwise the weighted mix decides.
fn choose_op(state: &State, rng: &mut SmallRng, weights: &[(Op, u32)]) -> Op {
    let tracked = state.tracked_bytes.load(Ordering::Relaxed);
    let low = (state.target_size as f64 * 0.85) as u64;
    let high = (state.target_size as f64 * 0.95) as u64;
    if tracked < low {
        Op::PutNew
    } else if tracked > high {
        Op::Delete
    } else {
        pick_op(rng, weights)
    }
}

async fn steady_churn(state: Arc<State>, cli: &Cli, workers: usize, deadline: Instant) -> Result<()> {
    let mix = parse_size_mix(&cli.file_size_mix)?;
    let weights = base_weights(cli.enable_embed, cli.enable_versions, cli.admin_token.is_some());
    eprintln!(
        "[stability] phase 2: steady churn until {:?} remaining, {} ops in menu",
        deadline.saturating_duration_since(Instant::now()),
        weights.len()
    );
    let mut handles = Vec::with_capacity(workers);
    for w in 0..workers {
        let state = Arc::clone(&state);
        let mix = mix.clone();
        let weights = weights.clone();
        let seed = cli.seed.unwrap_or_else(rand::random).wrapping_add(1_000 + w as u64);
        handles.push(tokio::spawn(async move {
            let mut rng = SmallRng::seed_from_u64(seed);
            loop {
                if state.abort.is_cancelled() {
                    return;
                }
                if Instant::now() >= deadline {
                    return;
                }
                let op = choose_op(&state, &mut rng, &weights);
                if let Err(msg) = run_op(&state, &mut rng, &mix, op, w).await {
                    state.set_abort(msg);
                    return;
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    if state.abort.is_cancelled() {
        bail!("steady-churn aborted");
    }
    Ok(())
}

/// Executes one op. Returns `Err(msg)` iff the caller must abort the run.
async fn run_op(
    state: &State,
    rng: &mut SmallRng,
    mix: &[SizeBucket],
    op: Op,
    worker: usize,
) -> Result<(), String> {
    match op {
        Op::PutNew => op_put_new(state, rng, mix, worker).await,
        Op::PutReplace => op_put_replace(state, rng, mix).await,
        Op::Get => op_get(state, rng).await,
        Op::RangeGet => op_range_get(state, rng).await,
        Op::Delete => op_delete(state, rng).await,
        Op::Mkdir => op_mkdir(state, rng, worker).await,
        Op::Rmdir => op_rmdir(state, rng).await,
        Op::Mv => op_mv(state, rng, worker).await,
        Op::VersionsList => op_versions_list(state, rng).await,
        Op::RestoreVersion => op_restore_version(state, rng).await,
        Op::Search => op_search(state).await,
        Op::Similar => op_similar(state, rng).await,
        Op::Spotlight => op_spotlight(state, rng).await,
        Op::Stats => op_stats(state).await,
        Op::Health => op_health(state).await,
        Op::GcOrphans => op_gc_orphans(state).await,
    }
}

// ---- op implementations ----------------------------------------------------

async fn op_put_new(
    state: &State,
    rng: &mut SmallRng,
    mix: &[SizeBucket],
    worker: usize,
) -> Result<(), String> {
    let size = pick_size(rng, mix);
    let seq = state.put_seq.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}/w{:02}-{:07}.bin", state.root, worker, seq);
    let body = make_binary(rng, size);
    let sha = hex_of(&sha256(&body));
    let put_resp = match put_object(state, &name, body, "application/octet-stream").await {
        Ok((Outcome::Ok, resp)) => resp,
        Ok((Outcome::RaceExpected(_, s), _)) => {
            match s {
                400 => state.counters.race_400.fetch_add(1, Ordering::Relaxed),
                _ => state.counters.race_404.fetch_add(1, Ordering::Relaxed),
            };
            return Ok(());
        }
        Ok((Outcome::Abort(m), _)) => return Err(m),
        Err(e) => return Err(format!("put {name}: {e}")),
    };
    let obj = OwnedObject {
        name: name.clone(),
        size,
        sha256_hex: sha,
        kind: ObjKind::Binary,
        put_response: put_resp,
    };
    state.counters.put_new.fetch_add(1, Ordering::Relaxed);
    state.counters.bytes_put.fetch_add(size, Ordering::Relaxed);
    state.tracked_bytes.fetch_add(size, Ordering::Relaxed);
    // Immediate verify: SHA-check what we just wrote.
    match verify_get(state, &obj).await {
        Ok(Outcome::Ok) => {
            state.counters.get.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Outcome::RaceExpected(_, _)) => {
            // Extremely unlikely — someone else deleted our brand-new object between
            // our PUT and our verify. Not impossible under a full-mix workload, so
            // don't abort; just count as race.
            state.counters.race_404.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Outcome::Abort(m)) => return Err(m),
        Err(e) => return Err(format!("verify {}: {e}", obj.name)),
    }
    state.owned.write().await.insert(name, obj);
    Ok(())
}

async fn op_put_replace(state: &State, rng: &mut SmallRng, mix: &[SizeBucket]) -> Result<(), String> {
    // `checkout` (not `pick_owned`) so no other worker can grab this
    // same name for a concurrent op while we're in flight. Two
    // overlapping put_replace's on the same name would race on
    // `cat.insert(name, manifest)` inside the gateway — the catalog
    // is atomic but "which manifest wins" is undefined, and the
    // loser's post-PUT `verify_get` would decode the winner's
    // content and abort with a SHA-256 mismatch. That mismatch is a
    // test-model bug (concurrent writes to the same key have no
    // deterministic outcome), not a gateway bug.
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    let old_size = target.size;
    let new_size = pick_size(rng, mix);
    let body = make_binary(rng, new_size);
    let sha = hex_of(&sha256(&body));
    match put_object(state, &target.name, body, "application/octet-stream").await {
        Ok((Outcome::Ok, resp)) => {
            state.counters.put_replace.fetch_add(1, Ordering::Relaxed);
            state.counters.bytes_put.fetch_add(new_size, Ordering::Relaxed);
            state
                .tracked_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(old_size).saturating_add(new_size))
                })
                .ok();
            let obj = OwnedObject {
                name: target.name.clone(),
                size: new_size,
                sha256_hex: sha,
                kind: ObjKind::Binary,
                put_response: resp,
            };
            let verify = verify_get(state, &obj).await;
            release(state, obj).await;
            match verify {
                Ok(Outcome::Ok) => {
                    state.counters.get.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Outcome::RaceExpected(_, _)) => {
                    state.counters.race_404.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Outcome::Abort(m)) => return Err(m),
                Err(e) => return Err(format!("verify: {e}")),
            }
            Ok(())
        }
        Ok((Outcome::RaceExpected(_, s), _)) => {
            release(state, target).await;
            match s {
                400 => state.counters.race_400.fetch_add(1, Ordering::Relaxed),
                _ => state.counters.race_404.fetch_add(1, Ordering::Relaxed),
            };
            Ok(())
        }
        Ok((Outcome::Abort(m), _)) => {
            release(state, target).await;
            Err(m)
        }
        Err(e) => {
            let name = target.name.clone();
            release(state, target).await;
            Err(format!("put_replace {name}: {e}"))
        }
    }
}

async fn op_get(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    // checkout — see the comment on `op_put_replace`. GET also needs
    // exclusive ownership of the name for the round-trip so a
    // concurrent put_replace can't swap the manifest between our
    // catalog snapshot and our HTTP GET.
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    let verify = verify_get(state, &target).await;
    match verify {
        Ok(Outcome::Ok) => {
            release(state, target).await;
            state.counters.get.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok(Outcome::RaceExpected(_, _)) => {
            // 404 on a name we just checked out means the catalog is
            // missing the entry — either persistence bug or someone
            // deleted it out-of-band. Under checkout no other worker
            // owns this name, so this is an abort.
            let name = target.name.clone();
            release(state, target).await;
            Err(format!("get {name}: 404 but still owned (missing catalog entry?)"))
        }
        Ok(Outcome::Abort(m)) => {
            release(state, target).await;
            Err(m)
        }
        Err(e) => {
            let name = target.name.clone();
            release(state, target).await;
            Err(format!("get {name}: {e}"))
        }
    }
}

async fn op_range_get(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    if target.size < 128 {
        release(state, target).await;
        return Ok(());
    }
    let start = rng.gen_range(0..(target.size - 64));
    let end = start + rng.gen_range(1..64.min(target.size - start));
    let url = format!("{}/{}", state.base, target.name);
    let resp = state.client.get(&url).header("range", format!("bytes={start}-{end}")).send().await;
    let name = target.name.clone();
    release(state, target).await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.bytes().await.map_err(|e| format!("range_get body: {e}"))?;
            match status {
                200 | 206 => {
                    state.counters.range_get.fetch_add(1, Ordering::Relaxed);
                    state.counters.bytes_get.fetch_add(body.len() as u64, Ordering::Relaxed);
                    Ok(())
                }
                404 => Err(format!("range_get {name}: 404 but was still owned")),
                s => match classify_status("range_get", s, &body_hint(&body)) {
                    Outcome::Abort(m) => Err(m),
                    _ => Ok(()),
                },
            }
        }
        Err(e) => Err(format!("range_get {name}: transport ({e})")),
    }
}

async fn op_delete(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    let outcome = delete_object(state, &target.name).await;
    match outcome {
        Ok(Outcome::Ok) => {
            release_gone(state, &target.name).await;
            state.counters.delete.fetch_add(1, Ordering::Relaxed);
            state
                .tracked_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(target.size))
                })
                .ok();
            Ok(())
        }
        Ok(Outcome::RaceExpected(_, _)) => {
            // Under checkout no other worker owns this name, so a 404
            // means the catalog entry is missing without a deleter to
            // blame — treat as abort.
            let name = target.name.clone();
            release(state, target).await;
            Err(format!("delete {name}: 404 but still owned"))
        }
        Ok(Outcome::Abort(m)) => {
            release(state, target).await;
            Err(m)
        }
        Err(e) => {
            release(state, target).await;
            Err(format!("delete: {e}"))
        }
    }
}

async fn op_mkdir(state: &State, _rng: &mut SmallRng, worker: usize) -> Result<(), String> {
    let seq = state.put_seq.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}/dirs/d-w{worker:02}-{seq:06}", state.root);
    let url = format!("{}/api/mkdir/{}", state.base, name);
    match state.client.post(&url).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match classify_status("mkdir", status, &body_hint(body.as_bytes())) {
                Outcome::Ok => {
                    state.counters.mkdir.fetch_add(1, Ordering::Relaxed);
                    state.dirs.write().await.push(name);
                    Ok(())
                }
                Outcome::RaceExpected(_, _) => {
                    state.counters.race_409.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::Abort(m) => Err(m),
            }
        }
        Err(e) => Err(format!("mkdir {name}: transport ({e})")),
    }
}

async fn op_rmdir(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let name = {
        let mut dirs = state.dirs.write().await;
        if dirs.is_empty() {
            return Ok(());
        }
        let idx = rng.gen_range(0..dirs.len());
        dirs.swap_remove(idx)
    };
    let url = format!("{}/api/rmdir/{}", state.base, name);
    match state.client.delete(&url).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match classify_status("rmdir", status, &body_hint(body.as_bytes())) {
                Outcome::Ok => {
                    state.counters.rmdir.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::RaceExpected(_, _) => {
                    state.counters.race_409.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::Abort(m) => Err(m),
            }
        }
        Err(e) => Err(format!("rmdir {name}: transport ({e})")),
    }
}

async fn op_mv(state: &State, rng: &mut SmallRng, worker: usize) -> Result<(), String> {
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    let seq = state.put_seq.fetch_add(1, Ordering::Relaxed);
    let new_name = format!("{}/moved/w{worker:02}-{seq:06}.bin", state.root);
    let url = format!("{}/api/mv", state.base);
    let form = [("from", target.name.as_str()), ("to", new_name.as_str())];
    let resp = state.client.post(&url).form(&form).send().await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match classify_status("mv", status, &body_hint(body.as_bytes())) {
                Outcome::Ok => {
                    state.counters.mv.fetch_add(1, Ordering::Relaxed);
                    let old_name = target.name.clone();
                    let moved = OwnedObject { name: new_name.clone(), ..target };
                    release(state, moved).await;
                    release_gone(state, &old_name).await;
                    Ok(())
                }
                Outcome::RaceExpected(_, _) => {
                    release(state, target).await;
                    state.counters.race_404.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::Abort(m) => {
                    release(state, target).await;
                    Err(m)
                }
            }
        }
        Err(e) => {
            release(state, target).await;
            Err(format!("mv: transport ({e})"))
        }
    }
}

async fn op_versions_list(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    // Read-only op — release immediately so the name is visible to
    // pickers again. The version-list / restore path doesn't need
    // exclusive ownership since it doesn't mutate the primary manifest.
    // (These ops are not in the current mix anyway; kept here for
    // future re-enable.)
    let target_snapshot = target.clone();
    release(state, target).await;
    let target = target_snapshot;
    let url = format!("{}/api/versions?name={}", state.base, target.name);
    match http_get_text(&state.client, &url).await {
        Ok((status, body)) => match classify_status("versions_list", status, &body_hint(body.as_bytes())) {
            Outcome::Ok => {
                state.counters.versions_list.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Outcome::RaceExpected(_, _) => {
                state.counters.race_404.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Outcome::Abort(m) => Err(m),
        },
        Err(e) => Err(format!("versions_list: transport ({e})")),
    }
}

async fn op_restore_version(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    // Best-effort: pick an owned name, list versions, restore the oldest.
    let target = match checkout(state, rng).await {
        Some(o) => o,
        None => return Ok(()),
    };
    // Read-only op — release immediately so the name is visible to
    // pickers again. The version-list / restore path doesn't need
    // exclusive ownership since it doesn't mutate the primary manifest.
    // (These ops are not in the current mix anyway; kept here for
    // future re-enable.)
    let target_snapshot = target.clone();
    release(state, target).await;
    let target = target_snapshot;
    let list_url = format!("{}/api/versions?name={}", state.base, target.name);
    let (status, body) = match http_get_text(&state.client, &list_url).await {
        Ok(v) => v,
        Err(e) => return Err(format!("restore: list transport ({e})")),
    };
    if status == 404 {
        state.counters.race_404.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if status != 200 {
        return match classify_status("restore_version", status, &body_hint(body.as_bytes())) {
            Outcome::Abort(m) => Err(m),
            _ => Ok(()),
        };
    }
    let versions: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let ts_opt = versions
        .get("versions")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("t").or_else(|| entry.get("timestamp")))
        .and_then(|t| t.as_str().map(str::to_string));
    let Some(ts) = ts_opt else { return Ok(()) };
    let url = format!("{}/api/restore", state.base);
    let form = [("name", target.name.as_str()), ("timestamp", ts.as_str())];
    match state.client.post(&url).form(&form).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match classify_status("restore_version", status, &body_hint(body.as_bytes())) {
                Outcome::Ok => {
                    state.counters.restore_version.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::RaceExpected(_, _) => {
                    state.counters.race_404.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::Abort(m) => Err(m),
            }
        }
        Err(e) => Err(format!("restore_version: transport ({e})")),
    }
}

async fn op_search(state: &State) -> Result<(), String> {
    let url = format!("{}/api/search?q=colorful+pattern&k=5", state.base);
    match http_get_text(&state.client, &url).await {
        Ok((200, _)) => {
            state.counters.search.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok((s, body)) => match classify_status("search", s, &body_hint(body.as_bytes())) {
            Outcome::Abort(m) => Err(m),
            _ => {
                state.counters.race_404.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        },
        Err(e) => Err(format!("search: transport ({e})")),
    }
}

async fn op_similar(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let name = match pick_image(state, rng) {
        Some(n) => n,
        None => return Ok(()),
    };
    let url = format!("{}/api/similar/{}", state.base, name);
    match http_get_text(&state.client, &url).await {
        Ok((200, _)) => {
            state.counters.similar.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok((s, body)) => match classify_status("similar", s, &body_hint(body.as_bytes())) {
            Outcome::Abort(m) => Err(m),
            _ => Ok(()),
        },
        Err(e) => Err(format!("similar: transport ({e})")),
    }
}

async fn op_spotlight(state: &State, rng: &mut SmallRng) -> Result<(), String> {
    let name = match pick_image(state, rng) {
        Some(n) => n,
        None => return Ok(()),
    };
    let url = format!("{}/api/spotlight.png?name={}", state.base, name);
    match http_get_bytes(&state.client, &url).await {
        Ok((200, _bytes)) => {
            state.counters.spotlight.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok((s, body)) => match classify_status("spotlight", s, &body_hint(&body)) {
            Outcome::Abort(m) => Err(m),
            _ => Ok(()),
        },
        Err(e) => Err(format!("spotlight: transport ({e})")),
    }
}

async fn op_stats(state: &State) -> Result<(), String> {
    let url = format!("{}/api/stats", state.base);
    match http_get_text(&state.client, &url).await {
        Ok((200, _)) => {
            state.counters.stats.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok((s, body)) => match classify_status("stats", s, &body_hint(body.as_bytes())) {
            Outcome::Abort(m) => Err(m),
            _ => Ok(()),
        },
        Err(e) => Err(format!("stats: transport ({e})")),
    }
}

async fn op_health(state: &State) -> Result<(), String> {
    let url = format!("{}/metrics", state.base);
    match http_get_text(&state.client, &url).await {
        Ok((200, _)) => {
            state.counters.health.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok((s, body)) => match classify_status("health", s, &body_hint(body.as_bytes())) {
            Outcome::Abort(m) => Err(m),
            _ => Ok(()),
        },
        Err(e) => Err(format!("health: transport ({e})")),
    }
}

async fn op_gc_orphans(state: &State) -> Result<(), String> {
    let url = format!("{}/api/gc", state.base);
    let mut req = state.client.post(&url);
    if !state.admin_token.is_empty() {
        req = req.bearer_auth(&state.admin_token);
    }
    match req.send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match classify_status("gc_orphans", status, &body_hint(body.as_bytes())) {
                Outcome::Ok => {
                    state.counters.gc_orphans.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Outcome::Abort(m) => Err(m),
                Outcome::RaceExpected(_, _) => Ok(()),
            }
        }
        Err(e) => Err(format!("gc_orphans: transport ({e})")),
    }
}

// ============================================================================
// Owned-map helpers
// ============================================================================

/// Atomically pop an object from `owned` and mark it in-flight. No
/// other worker can pick this name until [`release`] or [`release_gone`]
/// runs — closes the "two workers concurrently mutate the same catalog
/// entry" race that produces spurious SHA-256 mismatches.
///
/// Both mutating ops (put_replace / delete / mv) and read-only ops
/// (get / range_get / versions_list) use this — a concurrent
/// put_replace can flip the underlying manifest between a read op's
/// snapshot and its GET, so readers need the same guarantee.
async fn checkout(state: &State, rng: &mut SmallRng) -> Option<OwnedObject> {
    // Hold the write-lock across the pop and the in_flight insert so
    // there's no interleaving where two workers see the same name in
    // owned and both grab it. Order: owned → in_flight → drop locks.
    let (name, obj) = {
        let mut owned = state.owned.write().await;
        if owned.is_empty() {
            return None;
        }
        let key = {
            let idx = rng.gen_range(0..owned.len());
            owned.keys().nth(idx).cloned()?
        };
        let obj = owned.remove(&key)?;
        (key, obj)
    };
    state.in_flight.write().await.insert(name);
    Some(obj)
}

/// Restore a checked-out object back into `owned` (content unchanged
/// or updated to `obj`). Complement to [`checkout`].
async fn release(state: &State, obj: OwnedObject) {
    let name = obj.name.clone();
    state.owned.write().await.insert(name.clone(), obj);
    state.in_flight.write().await.remove(&name);
}

/// Release a checked-out name that no longer exists (successful DELETE
/// or MV-away). The name is dropped from `in_flight` without going
/// back into `owned`.
async fn release_gone(state: &State, name: &str) {
    state.in_flight.write().await.remove(name);
}

fn pick_image(state: &State, rng: &mut SmallRng) -> Option<String> {
    if state.image_names.is_empty() {
        return None;
    }
    let idx = rng.gen_range(0..state.image_names.len());
    Some(state.image_names[idx].clone())
}

// ============================================================================
// Image pool (search/similar/spotlight targets)
// ============================================================================

async fn seed_image_pool(state: &Arc<State>) -> Result<Vec<String>> {
    let count: u64 = 20;
    let mut names = Vec::with_capacity(count as usize);
    for i in 0..count {
        let name = format!("{}/images/img-{:03}.png", state.root, i);
        let body = make_image(i);
        match put_object(state, &name, body, "image/png").await {
            Ok((Outcome::Ok, _)) => {
                names.push(name);
            }
            Ok((o, _)) => bail!("seed image {name}: {o:?}"),
            Err(e) => bail!("seed image {name}: {e}"),
        }
    }
    // Trigger embed index build so search/similar/spotlight has coverage.
    let url = format!("{}/api/embed_all", state.base);
    let _ = state.client.post(&url).send().await;
    Ok(names)
}

// ============================================================================
// Phase 3: drain
// ============================================================================

async fn drain(state: Arc<State>, workers: usize) -> Result<()> {
    let start_owned = state.owned.read().await.len();
    let start_dirs = state.dirs.read().await.len();
    eprintln!(
        "[stability] phase 3: draining {} objects + {} dirs + {} images (batch delete via POST /api/batch/delete)",
        start_owned,
        start_dirs,
        state.image_names.len(),
    );
    // v4: bulk delete via `POST /api/batch/delete` — the per-object
    // DELETE path used to take ~2.5 h to drain 25 k objects (each
    // trigger a full-catalog O(N) scan inside `purge_orphans_of`).
    // The batch endpoint drops the whole set under one catalog write-
    // lock, one persist_catalog fsync, and one fanout PurgeByHash.
    //
    // Chunk size caps the write-lock hold at ~1 s per batch even on
    // slow disks; the server hard-caps at 10 000 names.
    const BATCH_CAP: usize = 1_000;
    let all_names: Vec<String> = {
        let mut owned = state.owned.write().await;
        owned.drain().map(|(k, _)| k).collect()
    };
    let mut handles = Vec::with_capacity(workers.min(1).max(1));
    // Split the name list into `workers` interleaved chunks so we
    // still get parallel HTTP round-trips against the gateway.
    let chunks: Vec<Vec<String>> = {
        let mut buckets: Vec<Vec<String>> = (0..workers).map(|_| Vec::new()).collect();
        for (i, name) in all_names.into_iter().enumerate() {
            buckets[i % workers].push(name);
        }
        buckets
    };
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let state = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            for slice in chunk.chunks(BATCH_CAP) {
                let url = format!("{}/api/batch/delete", state.base);
                let body = serde_json::json!({"names": slice});
                let resp = state.client.post(&url).json(&body).send().await;
                match resp {
                    Ok(r) => {
                        let status = r.status().as_u16();
                        if status == 200 {
                            if let Ok(v) = r.json::<serde_json::Value>().await {
                                let removed = v.get("removed").and_then(serde_json::Value::as_u64).unwrap_or(0);
                                state.counters.delete.fetch_add(removed, Ordering::Relaxed);
                            }
                        } else {
                            eprintln!("[stability] drain: batch_delete → HTTP {status}");
                        }
                    }
                    Err(e) => eprintln!("[stability] drain: batch_delete transport ({e})"),
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    // Best-effort: rmdir every directory we created. Skip on failure — the
    // final /api/stats check is the real verifier.
    // rmdir every subdirectory the workload created via op_mkdir. The
    // four setup-time subdirs (`<root>` + dirs/moved/images) are LEFT
    // in place — they were counted into the baseline snapshot so a
    // final `objects_total == baseline_objects` check passes on a
    // clean run.
    let dirs = std::mem::take(&mut *state.dirs.write().await);
    for d in dirs {
        let _ = state.client.delete(format!("{}/api/rmdir/{}", state.base, d)).send().await;
    }
    // Drop the image pool via the same batch endpoint if there is one.
    if !state.image_names.is_empty() {
        let url = format!("{}/api/batch/delete", state.base);
        let body = serde_json::json!({"names": state.image_names});
        let _ = state.client.post(&url).json(&body).send().await;
    }
    Ok(())
}

// ============================================================================
// Summary + baseline check
// ============================================================================

#[derive(Serialize)]
struct Summary {
    ended_at: String,
    duration_secs: u64,
    workers: usize,
    target_size_bytes: u64,
    tracked_bytes_final: u64,
    baseline_objects: u64,
    final_objects: u64,
    aborted: bool,
    abort_reason: Option<String>,
    ops: serde_json::Value,
    races: serde_json::Value,
    /// `objects_by_kind` snapshot captured right after churn ends but
    /// *before* drain runs — for diagnosing auto-detect misfires.
    churn_end_kind_counts: Option<serde_json::Value>,
    final_kind_counts: Option<serde_json::Value>,
}

async fn read_objects_total(client: &reqwest::Client, base: &str) -> Result<u64> {
    let (_status, body) = http_get_text(client, &format!("{base}/api/stats")).await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Ok(v.get("objects_total").and_then(serde_json::Value::as_u64).unwrap_or(0))
}

fn snapshot_counters(c: &Counters) -> (serde_json::Value, serde_json::Value) {
    let ops = json!({
        "put_new": c.put_new.load(Ordering::Relaxed),
        "put_replace": c.put_replace.load(Ordering::Relaxed),
        "get": c.get.load(Ordering::Relaxed),
        "range_get": c.range_get.load(Ordering::Relaxed),
        "delete": c.delete.load(Ordering::Relaxed),
        "mkdir": c.mkdir.load(Ordering::Relaxed),
        "rmdir": c.rmdir.load(Ordering::Relaxed),
        "mv": c.mv.load(Ordering::Relaxed),
        "versions_list": c.versions_list.load(Ordering::Relaxed),
        "restore_version": c.restore_version.load(Ordering::Relaxed),
        "search": c.search.load(Ordering::Relaxed),
        "similar": c.similar.load(Ordering::Relaxed),
        "spotlight": c.spotlight.load(Ordering::Relaxed),
        "stats": c.stats.load(Ordering::Relaxed),
        "health": c.health.load(Ordering::Relaxed),
        "gc_orphans": c.gc_orphans.load(Ordering::Relaxed),
        "bytes_put": c.bytes_put.load(Ordering::Relaxed),
        "bytes_get": c.bytes_get.load(Ordering::Relaxed),
    });
    let races = json!({
        "race_404": c.race_404.load(Ordering::Relaxed),
        "race_409": c.race_409.load(Ordering::Relaxed),
        "race_400": c.race_400.load(Ordering::Relaxed),
    });
    (ops, races)
}

// ============================================================================
// Entry point
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let duration = parse_duration(&cli.duration)?;
    let target_size = parse_size(&cli.target_size)?;
    let request_timeout = parse_duration(&cli.request_timeout)?;
    let workers = ((cli.encode as f64 * 0.8).floor() as usize).max(1);

    let run_id = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let run_dir = cli.out.join(&run_id);
    std::fs::create_dir_all(&run_dir)?;
    eprintln!("[stability] run {run_id} → {}", run_dir.display());
    eprintln!(
        "[stability] duration={:?} target={} MiB workers={} (encode={})",
        duration,
        target_size / 1024 / 1024,
        workers,
        cli.encode
    );

    let gw = spawn_gateway(&cli, &run_dir).await?;

    let client = reqwest::Client::builder()
        .timeout(request_timeout)
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;
    let root = format!("stability/{run_id}");
    // The gateway rejects PUT into a non-existent parent, so create the
    // full prefix chain before anything else runs. Uses the JSON /*path
    // variant (`POST /api/mkdir/<path>`) — the form variant expects
    // `parent=&name=` which is awkward for a nested chain. Any non-2xx
    // (except 409 = already exists) is a hard boot failure.
    for path in ["stability", &root, &format!("{root}/dirs"), &format!("{root}/moved"), &format!("{root}/images")] {
        let url = format!("{}/api/mkdir/{}", gw.base_url, path);
        let r = client.post(&url).send().await.with_context(|| format!("initial mkdir {path}"))?;
        let status = r.status().as_u16();
        if !(200..300).contains(&status) && status != 409 {
            let body = r.text().await.unwrap_or_default();
            bail!("initial mkdir {path} returned {status}: {}", body.chars().take(200).collect::<String>());
        }
    }
    // Baseline is captured AFTER the setup mkdirs — those five directory
    // markers are considered part of the steady state we're testing
    // stability against, and the drain phase does not remove them. The
    // final `objects_total` check compares against this number.
    //
    // `/api/stats` caches for 2 s (deliberate — dashboards + soak use
    // it), so we wait past the TTL before snapshotting so the baseline
    // reflects the post-mkdir catalog size and not a stale pre-mkdir
    // value. Same reason we let the run settle before the final read.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let baseline_objects = read_objects_total(&client, &gw.base_url).await?;

    let state = Arc::new(State {
        owned: RwLock::new(HashMap::new()),
        in_flight: RwLock::new(std::collections::HashSet::new()),
        image_names: Vec::new(),
        dirs: RwLock::new(Vec::new()),
        tracked_bytes: AtomicU64::new(0),
        baseline_objects,
        target_size,
        counters: Counters::default(),
        abort: CancellationToken::new(),
        abort_reason: std::sync::Mutex::new(None),
        put_seq: AtomicU64::new(0),
        root: root.clone(),
        base: gw.base_url.clone(),
        client: client.clone(),
        admin_token: cli.admin_token.clone().unwrap_or_default(),
    });

    // Seed the image pool (search/similar/spotlight targets) — mutable
    // because we bake the resulting names into the shared state. We wrap
    // in an Option so the `state` we push to workers already has the
    // final list.
    let images = if cli.enable_embed {
        seed_image_pool(&state).await?
    } else {
        Vec::new()
    };
    let state = {
        let mut s = Arc::try_unwrap(state).unwrap_or_else(|_| panic!("state Arc leaked before setup"));
        s.image_names = images;
        Arc::new(s)
    };

    let t_start = Instant::now();
    let deadline = t_start + duration;

    // Phase 1: ramp fill — abort budget ≤ 25% of --duration.
    let ramp_deadline = t_start + duration / 4;
    tokio::select! {
        r = ramp_fill(Arc::clone(&state), &cli, workers) => r?,
        _ = tokio::time::sleep_until(ramp_deadline) => {
            state.set_abort(format!("ramp-fill did not reach 0.9× target within {:?}", duration / 4));
        }
    }

    if !state.abort.is_cancelled() {
        // Phase 2: steady churn until `deadline`, minus a drain reserve
        // that scales with total duration: 5 min for a 24h run, 15 s
        // for a short smoke test. Guarantees drain has budget on short
        // runs without eating into a 24 h steady-state window.
        let drain_slack = duration
            .mul_f64(0.05)
            .max(Duration::from_secs(15))
            .min(Duration::from_secs(300));
        let churn_deadline = if deadline > Instant::now() + drain_slack {
            deadline - drain_slack
        } else {
            Instant::now()
        };
        let _ = steady_churn(Arc::clone(&state), &cli, workers, churn_deadline).await;
    }

    // Snapshot `objects_by_kind` right after churn ends and *before*
    // the drain phase wipes everything. This tells a post-mortem
    // whether the auto-detect misfired (audio/image/text > 0 when all
    // our PUTs should have gone opaque) — the info is destroyed once
    // drain runs, hence the pre-drain snapshot.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let churn_end_kind_counts = http_get_text(&client, &format!("{}/api/stats", gw.base_url))
        .await
        .ok()
        .and_then(|(_s, body)| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|v| v.get("objects_by_kind").cloned());

    // Phase 3: drain regardless of abort — we still want the cluster
    // returned to baseline for the objects_total check.
    let _ = drain(Arc::clone(&state), workers).await;

    // Post-conditions. Same 2 s TTL wait as at baseline capture.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let final_objects = read_objects_total(&client, &gw.base_url).await.unwrap_or(u64::MAX);
    // Snapshot `objects_by_kind` alongside the total so a post-mortem
    // can distinguish "opaque round-trip failed" from "auto-detect
    // misfired and stored as lossy audio/image/text".
    let final_kind_counts = http_get_text(&client, &format!("{}/api/stats", gw.base_url))
        .await
        .ok()
        .and_then(|(_s, body)| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|v| v.get("objects_by_kind").cloned());
    let aborted = state.abort.is_cancelled();
    let abort_reason = state.abort_reason.lock().ok().and_then(|g| g.clone());

    let (ops_json, races_json) = snapshot_counters(&state.counters);
    let summary = Summary {
        ended_at: chrono::Utc::now().to_rfc3339(),
        duration_secs: t_start.elapsed().as_secs(),
        workers,
        target_size_bytes: target_size,
        tracked_bytes_final: state.tracked_bytes.load(Ordering::Relaxed),
        baseline_objects,
        final_objects,
        aborted,
        abort_reason: abort_reason.clone(),
        ops: ops_json,
        races: races_json,
        churn_end_kind_counts,
        final_kind_counts,
    };
    let path = run_dir.join("summary.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
    eprintln!("[stability] summary → {}", path.display());

    gw.shutdown().await;

    if aborted {
        eprintln!("[stability] FAILED: {}", abort_reason.unwrap_or_else(|| "unknown".into()));
        std::process::exit(1);
    }
    if final_objects != baseline_objects {
        eprintln!(
            "[stability] FAILED: objects_total drift — baseline={baseline_objects}, final={final_objects}"
        );
        std::process::exit(2);
    }
    eprintln!("[stability] PASS");
    Ok(())
}
