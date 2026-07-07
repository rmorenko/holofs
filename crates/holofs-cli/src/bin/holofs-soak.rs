//! holofs-soak — long-running randomised-operation driver against a
//! live gateway. Assumes the cluster is already up and seeded (e.g.
//! via `make dev` + `dev-seed.sh`); we only exercise the HTTP surface
//! and record what breaks, so a subsequent analysis pass can turn a
//! multi-hour run into a bug list.
//!
//! Artefacts live under `<out>/<utc-timestamp>/`:
//! - `config.json`         — parameters + git-ish version + start time
//! - `ops.jsonl`           — one line per HTTP op: `{t, worker, op, target, http, ms, err?}`
//! - `errors.jsonl`        — filtered ops with `http >= 500` or `err`
//! - `metrics.jsonl`       — `{t, stats: {...}, prom: "<text>"}` snapshot every N s
//! - `health-events.jsonl` — raw SSE frames from `/api/health/events`
//! - `summary.json`        — final rollup: per-op counts, p50/p95 ms, error rate
//!
//! Ctrl-C stops the run cleanly and still writes `summary.json`.

#![allow(
    clippy::uninlined_format_args,
    clippy::format_in_format_args,
    clippy::items_after_statements,
    clippy::unreadable_literal,
    clippy::str_to_string,
    clippy::redundant_pub_crate,
    clippy::ignored_unit_patterns
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser, Debug, Clone)]
#[command(
    name = "holofs-soak",
    about = "Randomised long-running driver against a live holofs gateway.",
    version
)]
struct Cli {
    /// Gateway base URL (e.g. `http://127.0.0.1:8787`).
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    base: String,

    /// Number of parallel worker tasks.
    #[arg(long, default_value_t = 50)]
    workers: usize,

    /// Total run duration. Accepts `8h`, `30m`, `90s`, `2h30m`, `1h30m45s`.
    #[arg(long, default_value = "8h")]
    duration: String,

    /// Metrics snapshot interval.
    #[arg(long, default_value = "10s")]
    metrics_interval: String,

    /// Artefact root directory. A subdirectory `<utc-timestamp>/` is
    /// created inside it.
    #[arg(long, default_value = ".soak")]
    out: PathBuf,

    /// Seed for the master RNG (each worker derives its own from this).
    /// Omit to seed from the OS.
    #[arg(long)]
    seed: Option<u64>,

    /// Admin bearer token, if the gateway has `HOLOFS_ADMIN_TOKEN` set.
    /// Only admin routes need it; we currently do not call any.
    #[arg(long, env = "HOLOFS_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Per-request HTTP timeout.
    #[arg(long, default_value = "30s")]
    request_timeout: String,
}

// ============================================================================
// Duration parsing (supports concatenated `2h30m15s` groups)
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
        let n: u64 = num
            .parse()
            .with_context(|| format!("bad number in duration {s:?}"))?;
        num.clear();
        let ms = match ch {
            'h' | 'H' => n.checked_mul(3_600_000),
            'm' | 'M' => n.checked_mul(60_000),
            's' | 'S' => n.checked_mul(1_000),
            'd' | 'D' => n.checked_mul(86_400_000),
            other => bail!("unknown duration unit {other:?} in {s:?}"),
        }
        .ok_or_else(|| anyhow::anyhow!("duration overflow in {s:?}"))?;
        total_ms = total_ms
            .checked_add(ms)
            .ok_or_else(|| anyhow::anyhow!("duration overflow in {s:?}"))?;
    }
    if !num.is_empty() {
        // Bare number → seconds.
        let n: u64 = num.parse()?;
        total_ms = total_ms.saturating_add(n.saturating_mul(1_000));
    }
    Ok(Duration::from_millis(total_ms))
}

// ============================================================================
// Op kinds + weights
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Op {
    GetRandom,
    PutNew,
    PutReplace,
    Delete,
    Mkdir,
    Rmdir,
    Mv,
    Search,
    Similar,
    Spotlight,
    Health,
    Stats,
    VersionsList,
    RangeGet,
}

impl Op {
    const ALL: &'static [(Op, u32)] = &[
        (Op::GetRandom, 30),
        (Op::PutNew, 15),
        (Op::PutReplace, 10),
        (Op::Search, 8),
        (Op::Similar, 5),
        (Op::Spotlight, 5),
        (Op::Health, 5),
        (Op::Stats, 5),
        (Op::VersionsList, 4),
        (Op::Delete, 4),
        (Op::RangeGet, 3),
        (Op::Mkdir, 2),
        (Op::Rmdir, 2),
        (Op::Mv, 2),
    ];

    fn label(self) -> &'static str {
        match self {
            Op::GetRandom => "get_random",
            Op::PutNew => "put_new",
            Op::PutReplace => "put_replace",
            Op::Delete => "delete",
            Op::Mkdir => "mkdir",
            Op::Rmdir => "rmdir",
            Op::Mv => "mv",
            Op::Search => "search",
            Op::Similar => "similar",
            Op::Spotlight => "spotlight",
            Op::Health => "health",
            Op::Stats => "stats",
            Op::VersionsList => "versions_list",
            Op::RangeGet => "range_get",
        }
    }
}

fn pick_op(rng: &mut SmallRng) -> Op {
    let total: u32 = Op::ALL.iter().map(|(_, w)| *w).sum();
    let mut r = rng.gen_range(0..total);
    for (op, w) in Op::ALL {
        if r < *w {
            return *op;
        }
        r -= *w;
    }
    Op::GetRandom
}

// ============================================================================
// Op record (one HTTP call outcome)
// ============================================================================

#[derive(Debug, Serialize)]
struct OpRecord {
    t: String,      // ISO-8601 UTC
    worker: usize,  // worker index
    op: &'static str,
    target: String, // path / name touched
    http: u16,      // 0 for transport-level error
    ms: u64,        // total wall time
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

// ============================================================================
// Live catalog (best-effort; races with other workers are tolerated)
// ============================================================================

#[derive(Default)]
struct LiveSet {
    files: RwLock<Vec<String>>,      // decodable object names (non-directory)
    dirs: RwLock<Vec<String>>,       // directory names
    put_counter: AtomicU64,          // monotonic suffix for generated names
}

impl LiveSet {
    async fn random_file(&self, rng: &mut SmallRng) -> Option<String> {
        let g = self.files.read().await;
        g.choose(rng).cloned()
    }
    async fn random_dir(&self, rng: &mut SmallRng) -> Option<String> {
        let g = self.dirs.read().await;
        g.choose(rng).cloned()
    }
    async fn add_file(&self, name: String) {
        let mut g = self.files.write().await;
        if !g.iter().any(|n| n == &name) {
            g.push(name);
        }
    }
    async fn add_dir(&self, name: String) {
        let mut g = self.dirs.write().await;
        if !g.iter().any(|n| n == &name) {
            g.push(name);
        }
    }
    async fn remove_file(&self, name: &str) {
        let mut g = self.files.write().await;
        g.retain(|n| n != name);
    }
    async fn remove_dir(&self, name: &str) {
        let mut g = self.dirs.write().await;
        g.retain(|n| n != name);
    }
    async fn size(&self) -> (usize, usize) {
        let f = self.files.read().await.len();
        let d = self.dirs.read().await.len();
        (f, d)
    }
}

// ============================================================================
// Initial catalog bootstrap via /api/get_catalog
// ============================================================================

#[derive(Debug, Deserialize)]
struct CatalogRow {
    name: String,
    kind: String,
}

async fn bootstrap_live_set(base: &str, client: &reqwest::Client) -> Result<LiveSet> {
    let url = format!("{base}/api/get_catalog");
    // Leptos server-fn requires all three fields; empty strings mean "no filter".
    let form = [("name_glob", ""), ("date_from", ""), ("date_to", "")];
    let resp = client
        .post(&url)
        .form(&form)
        .send()
        .await
        .with_context(|| format!("bootstrap: POST {url}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("bootstrap /api/get_catalog → HTTP {status}: {body}");
    }
    let rows: Vec<CatalogRow> = serde_json::from_str(&body)
        .with_context(|| format!("bootstrap: parse catalog JSON ({} B)", body.len()))?;
    let live = LiveSet::default();
    for row in rows {
        if row.kind == "directory" {
            live.add_dir(row.name).await;
        } else {
            live.add_file(row.name).await;
        }
    }
    Ok(live)
}

// ============================================================================
// Tiny random-content PNG generator (no external files, unique bytes)
// ============================================================================

fn random_png(rng: &mut SmallRng) -> Vec<u8> {
    // 32×32 RGB with random per-pixel jitter. Cheap and each PUT differs.
    const W: u32 = 32;
    const H: u32 = 32;
    let mut buf = Vec::with_capacity(W as usize * H as usize * 3);
    let base_r: u8 = rng.gen();
    let base_g: u8 = rng.gen();
    let base_b: u8 = rng.gen();
    for _ in 0..(W * H) {
        buf.push(base_r.wrapping_add(rng.gen_range(0..16)));
        buf.push(base_g.wrapping_add(rng.gen_range(0..16)));
        buf.push(base_b.wrapping_add(rng.gen_range(0..16)));
    }
    let mut out = Vec::with_capacity(4096);
    {
        let mut enc = png::Encoder::new(&mut out, W, H);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&buf).expect("png data");
    }
    out
}

// ============================================================================
// One HTTP call → OpRecord
// ============================================================================

async fn do_one(
    op: Op,
    worker_id: usize,
    base: &str,
    client: &reqwest::Client,
    live: &LiveSet,
    rng: &mut SmallRng,
) -> OpRecord {
    let start = Instant::now();
    let ts_iso = chrono::Utc::now().to_rfc3339();
    let (target, http, err) = match op {
        Op::GetRandom => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let url = format!("{base}/{}", encode_path(&name));
            match client.get(&url).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::RangeGet => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let start_b = rng.gen_range(0..1024);
            let end_b = start_b + rng.gen_range(64..1024);
            let url = format!("{base}/{}", encode_path(&name));
            let req = client.get(&url).header("Range", format!("bytes={start_b}-{end_b}"));
            match req.send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::PutNew => {
            let n = live.put_counter.fetch_add(1, Ordering::Relaxed);
            let dir_pick = live.random_dir(rng).await;
            let path = match dir_pick {
                Some(d) if !d.is_empty() => format!("{d}/soak-w{worker_id:02}-{n:06}.png"),
                _ => format!("soak-w{worker_id:02}-{n:06}.png"),
            };
            let body = random_png(rng);
            let url = format!("{base}/{}", encode_path(&path));
            match client.put(&url).body(body).send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    if (200..300).contains(&s) {
                        live.add_file(path.clone()).await;
                    }
                    (path, s, None)
                }
                Err(e) => (path, 0, Some(e.to_string())),
            }
        }
        Op::PutReplace => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let body = random_png(rng);
            let url = format!("{base}/{}", encode_path(&name));
            match client.put(&url).body(body).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Delete => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let url = format!("{base}/{}", encode_path(&name));
            match client.delete(&url).send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    if (200..300).contains(&s) || s == 404 {
                        live.remove_file(&name).await;
                    }
                    (name, s, None)
                }
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Mkdir => {
            let n = live.put_counter.fetch_add(1, Ordering::Relaxed);
            let parent = live.random_dir(rng).await.unwrap_or_default();
            let name = format!("soak-dir-{n:06}");
            let form = [("parent", parent.as_str()), ("name", name.as_str())];
            let url = format!("{base}/api/mkdir");
            match client.post(&url).form(&form).send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    // form-friendly mkdir redirects on success (303).
                    if (200..400).contains(&s) {
                        let full = if parent.is_empty() {
                            name.clone()
                        } else {
                            format!("{parent}/{name}")
                        };
                        live.add_dir(full.clone()).await;
                        (full, s, None)
                    } else {
                        (name, s, None)
                    }
                }
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Rmdir => {
            let Some(dir) = live.random_dir(rng).await.filter(|d| !d.is_empty()) else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_dirs");
            };
            let url = format!("{base}/api/rmdir/{}", encode_path(&dir));
            match client.delete(&url).send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    // 409 is "not empty" — expected under concurrent writes.
                    if (200..300).contains(&s) || s == 404 {
                        live.remove_dir(&dir).await;
                    }
                    (dir, s, None)
                }
                Err(e) => (dir, 0, Some(e.to_string())),
            }
        }
        Op::Mv => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let n = live.put_counter.fetch_add(1, Ordering::Relaxed);
            let (parent, leaf) = match name.rsplit_once('/') {
                Some((p, l)) => (p.to_string(), l.to_string()),
                None => (String::new(), name.clone()),
            };
            let renamed = format!("moved-{n:06}-{leaf}");
            let new = if parent.is_empty() {
                renamed.clone()
            } else {
                format!("{parent}/{renamed}")
            };
            let form = [("from", name.as_str()), ("to", new.as_str())];
            let url = format!("{base}/api/mv");
            match client.post(&url).form(&form).send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    if (200..300).contains(&s) || s == 303 {
                        live.remove_file(&name).await;
                        live.add_file(new.clone()).await;
                    }
                    (format!("{name} → {new}"), s, None)
                }
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Search => {
            let q_words = ["mountain", "valley", "test", "logo", "photo", "abstract", "shore"];
            let q = q_words.choose(rng).copied().unwrap_or("test");
            let bands = ["any", "coarse", "mid", "full"];
            let band = bands.choose(rng).copied().unwrap_or("any");
            let url = format!("{base}/api/search?q={q}&limit=10&band={band}");
            match client.get(&url).send().await {
                Ok(r) => (format!("q={q}&band={band}"), r.status().as_u16(), None),
                Err(e) => (format!("q={q}"), 0, Some(e.to_string())),
            }
        }
        Op::Similar => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let form = [("name", name.as_str()), ("scope", "all")];
            let url = format!("{base}/api/similar");
            match client.post(&url).form(&form).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Spotlight => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let mode = if rng.gen_bool(0.5) { "spatial" } else { "coeff" };
            let url = format!(
                "{base}/api/spotlight.png?name={}&mode={mode}",
                url_form(&name)
            );
            match client.get(&url).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Health => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let url = format!("{base}/health/{}", encode_path(&name));
            match client.get(&url).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
        Op::Stats => {
            let url = format!("{base}/api/stats");
            match client.get(&url).send().await {
                Ok(r) => ("/api/stats".into(), r.status().as_u16(), None),
                Err(e) => ("/api/stats".into(), 0, Some(e.to_string())),
            }
        }
        Op::VersionsList => {
            let Some(name) = live.random_file(rng).await else {
                return skip_record(op.label(), worker_id, &ts_iso, "no_files_yet");
            };
            let form = [("name", name.as_str())];
            let url = format!("{base}/api/versions_list");
            match client.post(&url).form(&form).send().await {
                Ok(r) => (name, r.status().as_u16(), None),
                Err(e) => (name, 0, Some(e.to_string())),
            }
        }
    };
    OpRecord {
        t: ts_iso,
        worker: worker_id,
        op: op.label(),
        target,
        http,
        ms: start.elapsed().as_millis() as u64,
        err,
    }
}

fn skip_record(op: &'static str, worker: usize, ts: &str, reason: &str) -> OpRecord {
    OpRecord {
        t: ts.to_string(),
        worker,
        op,
        target: String::new(),
        http: 0,
        ms: 0,
        err: Some(format!("skip:{reason}")),
    }
}

fn encode_path(p: &str) -> String {
    // Preserve `/` but percent-escape query-unsafe chars in each segment.
    p.split('/')
        .map(|seg| {
            let mut out = String::with_capacity(seg.len());
            for b in seg.as_bytes() {
                match *b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        out.push(*b as char);
                    }
                    _ => {
                        use std::fmt::Write as _;
                        let _ = write!(out, "%{b:02X}");
                    }
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn url_form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

// ============================================================================
// Writer task — batches OpRecord → ops.jsonl, filters into errors.jsonl
// ============================================================================

struct Writers {
    ops: tokio::fs::File,
    errors: tokio::fs::File,
}

async fn writer_task(
    mut rx: mpsc::Receiver<OpRecord>,
    mut w: Writers,
    counters: Arc<Counters>,
) -> Result<()> {
    let mut batch: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut err_batch: Vec<u8> = Vec::with_capacity(4 * 1024);
    let mut last_flush = Instant::now();
    while let Some(rec) = rx.recv().await {
        counters.record(&rec);
        let is_error = rec.http >= 500
            || rec.err.as_ref().is_some_and(|e| !e.starts_with("skip:"));
        let line = serde_json::to_string(&rec).unwrap_or_else(|_| "\"encode-fail\"".into());
        batch.extend_from_slice(line.as_bytes());
        batch.push(b'\n');
        if is_error {
            err_batch.extend_from_slice(line.as_bytes());
            err_batch.push(b'\n');
        }
        // Flush on size or every 2 s.
        if batch.len() >= 8 * 1024 || last_flush.elapsed() >= Duration::from_secs(2) {
            w.ops.write_all(&batch).await?;
            batch.clear();
            if !err_batch.is_empty() {
                w.errors.write_all(&err_batch).await?;
                err_batch.clear();
            }
            last_flush = Instant::now();
        }
    }
    // Drain on shutdown.
    if !batch.is_empty() {
        w.ops.write_all(&batch).await?;
    }
    if !err_batch.is_empty() {
        w.errors.write_all(&err_batch).await?;
    }
    w.ops.flush().await?;
    w.errors.flush().await?;
    Ok(())
}

// ============================================================================
// In-memory rollup counters for summary.json
// ============================================================================

struct Counters {
    inner: std::sync::Mutex<CountersInner>,
}

#[derive(Default)]
struct CountersInner {
    per_op: BTreeMap<&'static str, PerOp>,
    total_ops: u64,
    total_errors: u64,
}

#[derive(Default, Clone)]
struct PerOp {
    n: u64,
    err: u64,
    // For p50/p95 we sample latencies via a reservoir (cap 4096).
    lat_sample: Vec<u64>,
    status_counts: BTreeMap<u16, u64>,
}

impl Counters {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(CountersInner::default()),
        })
    }

    fn record(&self, rec: &OpRecord) {
        let mut g = self.inner.lock().expect("counters mutex");
        g.total_ops += 1;
        let is_err = rec.http >= 500
            || rec.err.as_ref().is_some_and(|e| !e.starts_with("skip:"));
        if is_err {
            g.total_errors += 1;
        }
        let entry = g.per_op.entry(rec.op).or_default();
        entry.n += 1;
        if is_err {
            entry.err += 1;
        }
        *entry.status_counts.entry(rec.http).or_default() += 1;
        // Reservoir sample.
        if entry.lat_sample.len() < 4096 {
            entry.lat_sample.push(rec.ms);
        }
    }

    fn snapshot(&self) -> serde_json::Value {
        let g = self.inner.lock().expect("counters mutex");
        let mut ops = BTreeMap::new();
        for (name, po) in &g.per_op {
            let mut lat = po.lat_sample.clone();
            lat.sort_unstable();
            let p = |q: f64| -> u64 {
                if lat.is_empty() {
                    return 0;
                }
                let idx = ((lat.len() as f64) * q).clamp(0.0, (lat.len() - 1) as f64) as usize;
                lat[idx]
            };
            ops.insert(
                (*name).to_string(),
                json!({
                    "n": po.n,
                    "err": po.err,
                    "p50_ms": p(0.50),
                    "p95_ms": p(0.95),
                    "p99_ms": p(0.99),
                    "status": po.status_counts,
                }),
            );
        }
        json!({
            "total_ops": g.total_ops,
            "total_errors": g.total_errors,
            "error_rate": if g.total_ops == 0 { 0.0 } else { g.total_errors as f64 / g.total_ops as f64 },
            "per_op": ops,
        })
    }
}

// ============================================================================
// Worker
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn worker(
    id: usize,
    base: String,
    client: reqwest::Client,
    live: Arc<LiveSet>,
    tx: mpsc::Sender<OpRecord>,
    cancel: CancellationToken,
    seed: u64,
    thinktime_max_ms: u64,
) {
    let mut rng = SmallRng::seed_from_u64(seed);
    while !cancel.is_cancelled() {
        let op = pick_op(&mut rng);
        let rec = do_one(op, id, &base, &client, &live, &mut rng).await;
        // Send, tolerate closed channel on shutdown.
        if tx.send(rec).await.is_err() {
            break;
        }
        if thinktime_max_ms > 0 {
            let t = rng.gen_range(0..=thinktime_max_ms);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(t)) => {}
                _ = cancel.cancelled() => break,
            }
        }
    }
}

// ============================================================================
// Metrics collector — /metrics + /api/stats every N seconds
// ============================================================================

async fn metrics_collector(
    base: String,
    client: reqwest::Client,
    interval: Duration,
    mut file: tokio::fs::File,
    cancel: CancellationToken,
) -> Result<()> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the initial immediate tick.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let t = chrono::Utc::now().to_rfc3339();
                let prom = client
                    .get(format!("{base}/metrics"))
                    .send().await.ok();
                let stats = client
                    .get(format!("{base}/api/stats"))
                    .send().await.ok();
                let prom_text = if let Some(r) = prom {
                    if r.status().is_success() { r.text().await.unwrap_or_default() } else { String::new() }
                } else { String::new() };
                let stats_json = if let Some(r) = stats {
                    if r.status().is_success() {
                        r.json::<serde_json::Value>().await.ok()
                    } else { None }
                } else { None };
                let line = json!({
                    "t": t,
                    "stats": stats_json,
                    "prom": prom_text,
                });
                let mut buf = serde_json::to_vec(&line).unwrap_or_default();
                buf.push(b'\n');
                let _ = file.write_all(&buf).await;
            }
        }
    }
    file.flush().await.ok();
    Ok(())
}

// ============================================================================
// SSE consumer — /api/health/events
// ============================================================================

async fn sse_consumer(
    base: String,
    client: reqwest::Client,
    mut file: tokio::fs::File,
    cancel: CancellationToken,
) -> Result<()> {
    // Reconnect on drop; break on cancel.
    while !cancel.is_cancelled() {
        let req = client
            .get(format!("{base}/api/health/events"))
            .header("Accept", "text/event-stream");
        let resp = tokio::select! {
            r = req.send() => r,
            _ = cancel.cancelled() => break,
        };
        let Ok(resp) = resp else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        if !resp.status().is_success() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = tokio::select! {
            c = stream.next() => c,
            _ = cancel.cancelled() => None,
        } {
            let Ok(bytes) = chunk else { break };
            // Each SSE frame ends with \n\n; we log raw bytes with timestamps.
            let ts = chrono::Utc::now().to_rfc3339();
            let text = String::from_utf8_lossy(&bytes);
            for line in text.split('\n') {
                let s = line.trim_end_matches('\r');
                if s.is_empty() {
                    continue;
                }
                let line = json!({ "t": &ts, "raw": s });
                let mut buf = serde_json::to_vec(&line).unwrap_or_default();
                buf.push(b'\n');
                let _ = file.write_all(&buf).await;
            }
        }
        // Loop reconnects unless cancelled above.
    }
    file.flush().await.ok();
    Ok(())
}

// ============================================================================
// main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let duration = parse_duration(&cli.duration)?;
    let metrics_interval = parse_duration(&cli.metrics_interval)?;
    let request_timeout = parse_duration(&cli.request_timeout)?;

    if cli.workers == 0 {
        bail!("--workers must be > 0");
    }

    let run_id = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let run_dir = cli.out.join(&run_id);
    tokio::fs::create_dir_all(&run_dir).await?;

    let seed_master = cli.seed.unwrap_or_else(rand::random);
    let cfg = json!({
        "base": cli.base,
        "workers": cli.workers,
        "duration_ms": duration.as_millis() as u64,
        "metrics_interval_ms": metrics_interval.as_millis() as u64,
        "request_timeout_ms": request_timeout.as_millis() as u64,
        "seed": seed_master,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "run_id": run_id,
        "binary_version": env!("CARGO_PKG_VERSION"),
    });
    write_json(&run_dir.join("config.json"), &cfg).await?;

    let client = reqwest::Client::builder()
        .timeout(request_timeout)
        .pool_max_idle_per_host(cli.workers.min(64))
        .user_agent(concat!("holofs-soak/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Optional admin token wire-through (currently unused because no op
    // touches admin routes; keep the header for future opt-in).
    if cli.admin_token.is_some() {
        eprintln!("[soak] admin token present but no admin op is enabled");
    }

    eprintln!("[soak] run {run_id} → {}", run_dir.display());
    eprintln!("[soak] base={} workers={} duration={:?}", cli.base, cli.workers, duration);

    // Bootstrap live set.
    let live = Arc::new(bootstrap_live_set(&cli.base, &client).await?);
    let (files0, dirs0) = live.size().await;
    eprintln!("[soak] initial catalog: {files0} files, {dirs0} directories");
    if files0 == 0 {
        eprintln!("[soak] warning: empty catalog — put_new will still run, other ops will short-circuit");
    }

    // Files + channel.
    let ops_file = open_append(&run_dir.join("ops.jsonl")).await?;
    let errors_file = open_append(&run_dir.join("errors.jsonl")).await?;
    let metrics_file = open_append(&run_dir.join("metrics.jsonl")).await?;
    let health_file = open_append(&run_dir.join("health-events.jsonl")).await?;

    let counters = Counters::new();
    let (tx, rx) = mpsc::channel::<OpRecord>(cli.workers * 8);

    let cancel = CancellationToken::new();
    // Ctrl-C handler.
    let cancel_ctrlc = cancel.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            eprintln!("[soak] SIGINT — shutting down");
            cancel_ctrlc.cancel();
        }
    });
    // Deadline.
    let cancel_deadline = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        eprintln!("[soak] duration elapsed — shutting down");
        cancel_deadline.cancel();
    });

    // Spawn workers.
    let mut worker_handles = Vec::with_capacity(cli.workers);
    for i in 0..cli.workers {
        let base = cli.base.clone();
        let client = client.clone();
        let live = live.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        let seed = seed_master.wrapping_add(i as u64).wrapping_mul(0x9E3779B97F4A7C15);
        worker_handles.push(tokio::spawn(async move {
            worker(i, base, client, live, tx, cancel, seed, 50).await;
        }));
    }
    drop(tx); // writer will finish when all workers close their senders.

    // Writer.
    let writer_handle = tokio::spawn(writer_task(
        rx,
        Writers { ops: ops_file, errors: errors_file },
        counters.clone(),
    ));

    // Metrics + SSE.
    let metrics_handle = tokio::spawn(metrics_collector(
        cli.base.clone(),
        client.clone(),
        metrics_interval,
        metrics_file,
        cancel.clone(),
    ));
    let sse_handle = tokio::spawn(sse_consumer(
        cli.base.clone(),
        client.clone(),
        health_file,
        cancel.clone(),
    ));

    // Progress printer every 30 s.
    let cancel_prog = cancel.clone();
    let counters_prog = counters.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel_prog.cancelled() => break,
                _ = ticker.tick() => {
                    let g = counters_prog.inner.lock().expect("counters mutex");
                    eprintln!(
                        "[soak] progress: {} ops, {} errors ({:.3}%)",
                        g.total_ops,
                        g.total_errors,
                        if g.total_ops == 0 { 0.0 } else {
                            100.0 * g.total_errors as f64 / g.total_ops as f64
                        }
                    );
                }
            }
        }
    });

    // Wait for workers to notice cancel.
    for h in worker_handles {
        let _ = h.await;
    }
    let _ = writer_handle.await?;
    let _ = metrics_handle.await?;
    let _ = sse_handle.await?;

    // Final summary.
    let summary = counters.snapshot();
    let ended = chrono::Utc::now().to_rfc3339();
    let final_view = json!({
        "run_id": run_id,
        "ended_at": ended,
        "config": cfg,
        "rollup": summary,
    });
    write_json(&run_dir.join("summary.json"), &final_view).await?;

    eprintln!("[soak] done → {}", run_dir.display());
    Ok(())
}

// ============================================================================
// helpers
// ============================================================================

async fn open_append(path: &Path) -> Result<tokio::fs::File> {
    Ok(tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?)
}

async fn write_json(path: &Path, v: &serde_json::Value) -> Result<()> {
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    let s = serde_json::to_vec_pretty(v)?;
    f.write_all(&s).await?;
    f.write_all(b"\n").await?;
    f.flush().await?;
    Ok(())
}

// ============================================================================
// tests: duration parser
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("2h30m").unwrap(), Duration::from_secs(2 * 3600 + 30 * 60));
        assert_eq!(parse_duration("1h30m45s").unwrap(), Duration::from_secs(3600 + 30 * 60 + 45));
        assert_eq!(parse_duration("500").unwrap(), Duration::from_secs(500));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn duration_bad() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("30x").is_err());
    }

    #[test]
    fn ops_pick_deterministic() {
        let mut rng = SmallRng::seed_from_u64(42);
        let mut counts = BTreeMap::new();
        for _ in 0..1000 {
            *counts.entry(pick_op(&mut rng).label()).or_insert(0) += 1;
        }
        assert!(counts.contains_key("get_random"));
    }
}
