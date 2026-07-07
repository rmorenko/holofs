//! holofs-soak-report — turn a `.soak/<run>/` artefact directory into
//! a human-friendly report. Consumes `ops.jsonl`, `metrics.jsonl`,
//! `health-events.jsonl`, `config.json`, and `summary.json`; emits
//! either HTML (default — self-contained, inline CSS + SVG) or
//! GitHub-flavoured Markdown (`--format md`), or both.
//!
//! Streaming reader keeps peak RAM proportional to the number of
//! distinct timeseries buckets, not the raw ops count — an 8 h run
//! at 50 workers (~1.4 M ops, ~300 MB `ops.jsonl`) reports in a few
//! seconds and a couple hundred MB of RAM.

#![allow(
    clippy::uninlined_format_args,
    clippy::items_after_statements,
    clippy::unreadable_literal,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::str_to_string,
    clippy::format_in_format_args,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::needless_raw_string_hashes,
    clippy::redundant_pub_crate,
    clippy::ignored_unit_patterns,
    clippy::unnecessary_cast,
    clippy::single_match_else,
    clippy::stable_sort_primitive,
    clippy::unnecessary_wraps,
    clippy::single_char_add_str,
    clippy::write_with_newline,
    clippy::or_fun_call,
    dead_code
)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use clap::{Parser, ValueEnum};
use prometheus_parse::{Scrape, Value as PromValue};
use serde::Deserialize;

// ============================================================================
// CLI
// ============================================================================

#[derive(Copy, Clone, Debug, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum Format {
    Html,
    Md,
    Both,
}

#[derive(Parser, Debug)]
#[command(
    name = "holofs-soak-report",
    about = "Render a human-friendly report from a holofs-soak run directory.",
    version
)]
struct Cli {
    /// Path to a `.soak/<run>/` directory. Defaults to the most
    /// recent subdirectory of `./.soak/`.
    run: Option<PathBuf>,

    /// Output format. `both` writes `report.html` and `report.md`.
    #[arg(long, value_enum, default_value_t = Format::Html)]
    format: Format,

    /// Explicit output path. Default: `<run>/report.html` or
    /// `<run>/report.md` (or both, for `--format both`).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Time-series bucket size for timelines. Accepts `5m`, `30s`, `1h`.
    #[arg(long, default_value = "5m")]
    bucket: String,

    /// Roots of `.soak/` to scan when no run dir is given.
    #[arg(long, default_value = ".soak")]
    root: PathBuf,
}

// ============================================================================
// Duration parsing (subset shared with holofs-soak)
// ============================================================================

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty bucket size");
    }
    let mut num = String::new();
    let mut total_ms: u64 = 0;
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
            other => bail!("unknown duration unit {other:?} in {s:?}"),
        }
        .ok_or_else(|| anyhow!("duration overflow in {s:?}"))?;
        total_ms = total_ms
            .checked_add(ms)
            .ok_or_else(|| anyhow!("duration overflow in {s:?}"))?;
    }
    if !num.is_empty() {
        let n: u64 = num.parse()?;
        total_ms = total_ms.saturating_add(n.saturating_mul(1_000));
    }
    if total_ms == 0 {
        bail!("bucket size must be > 0");
    }
    Ok(Duration::from_millis(total_ms))
}

// ============================================================================
// Raw record types (mirror what holofs-soak writes)
// ============================================================================

#[derive(Debug, Deserialize)]
struct OpRecord {
    t: String,
    worker: usize,
    op: String,
    #[serde(default)]
    target: String,
    http: u16,
    ms: u64,
    #[serde(default)]
    err: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetricsSnapshot {
    t: String,
    #[serde(default)]
    stats: Option<serde_json::Value>,
    #[serde(default)]
    prom: String,
}

#[derive(Debug, Deserialize)]
struct HealthEnvelope {
    t: String,
    raw: String,
}

// ============================================================================
// Aggregation
// ============================================================================

/// Rollup for one operation kind.
#[derive(Debug, Default, Clone)]
struct OpAgg {
    n: u64,
    err: u64,        // http >= 500 OR non-`skip:` err string
    skipped: u64,    // `skip:no_files_yet` etc.
    ms_samples: Vec<u64>, // reservoir cap 20 000 per op
    status: BTreeMap<u16, u64>,
}

impl OpAgg {
    fn push(&mut self, rec: &OpRecord, is_err: bool, is_skip: bool) {
        self.n += 1;
        if is_err {
            self.err += 1;
        }
        if is_skip {
            self.skipped += 1;
        }
        *self.status.entry(rec.http).or_default() += 1;
        if self.ms_samples.len() < 20_000 {
            self.ms_samples.push(rec.ms);
        }
    }
    fn percentile(&mut self, q: f64) -> u64 {
        if self.ms_samples.is_empty() {
            return 0;
        }
        self.ms_samples.sort_unstable();
        let idx = ((self.ms_samples.len() as f64 - 1.0) * q).round() as usize;
        self.ms_samples[idx]
    }
    fn max_ms(&self) -> u64 {
        *self.ms_samples.iter().max().unwrap_or(&0)
    }
}

/// Per-bucket rollup (5 minutes by default).
#[derive(Default, Clone)]
struct BucketRollup {
    total_ops: u64,
    ok_ops: u64,           // 2xx/3xx
    client_errors: u64,    // 4xx (excluding 404 on GET-of-deleted, which is normal)
    server_errors: u64,    // 5xx
    transport_errors: u64, // http=0 + non-skip err
    // Latency histogram approximated via per-op samples merged later.
}

impl BucketRollup {
    fn merge(&mut self, other: &Self) {
        self.total_ops += other.total_ops;
        self.ok_ops += other.ok_ops;
        self.client_errors += other.client_errors;
        self.server_errors += other.server_errors;
        self.transport_errors += other.transport_errors;
    }
}

/// Per-worker rollup.
#[derive(Default, Clone)]
struct WorkerRollup {
    n: u64,
    err: u64,
}

/// Everything the renderers need.
struct Report {
    run_dir: PathBuf,
    config: serde_json::Value,
    summary: serde_json::Value,
    // Ops.
    per_op: BTreeMap<String, OpAgg>,
    total_ops: u64,
    total_errors: u64,
    total_skipped: u64,
    per_worker: BTreeMap<usize, WorkerRollup>,
    top_error_rows: Vec<TopErrorRow>,
    top_transport_msgs: Vec<(String, u64)>,
    // Timelines.
    bucket: Duration,
    bucket_seconds: u64,
    ops_timeline: BTreeMap<i64, BucketRollup>,
    per_op_latency_timeline: BTreeMap<String, BTreeMap<i64, Vec<u64>>>, // for hot ops
    // Cluster telemetry.
    stats_timeline: BTreeMap<i64, StatsSnapshot>,
    prom_timeline: BTreeMap<i64, PromSnapshot>,
    // Health events (limited to first N + last N buckets).
    health_events: Vec<(DateTime<Utc>, String)>,
    // Bounds for xaxis.
    t_min: Option<DateTime<Utc>>,
    t_max: Option<DateTime<Utc>>,
}

#[derive(Default, Clone)]
struct StatsSnapshot {
    nodes_live: u64,
    nodes_total: u64,
    objects_total: u64,
    shards_total: u64,
    shards_unique: u64,
    bytes_total: u64,
    auto_repairs_total: u64,
    auto_repair_failures_total: u64,
    scrub_runs_total: u64,
    scrub_repairs_total: u64,
}

#[derive(Default, Clone)]
struct PromSnapshot {
    backpressure_rejected_total: u64,
    handler_timeouts_total: u64,
    catalog_persist_failures_total: u64,
    supervised_task_restarts_total: u64,
    admin_auth_failures_total: u64,
    rate_limit_rejected_total: u64,
    backpressure_permits_available_medium: i64,
    backpressure_permits_available_long: i64,
}

/// One row in the "top errors" section.
#[derive(Debug, Clone)]
struct TopErrorRow {
    op: String,
    target: String,
    http: u16,
    count: u64,
}

// ============================================================================
// Ops classification
// ============================================================================

/// Which "bucket" of ops we sample per-bucket latency for. Reservoirs
/// are capped so the total memory footprint stays bounded even for
/// multi-hour runs.
const HOT_OPS: &[&str] = &[
    "get_random",
    "put_new",
    "put_replace",
    "delete",
    "search",
    "similar",
    "spotlight",
    "health",
    "stats",
    "versions_list",
    "range_get",
];

fn is_hot_op(name: &str) -> bool {
    HOT_OPS.contains(&name)
}

fn classify_error(rec: &OpRecord) -> (bool /*is_err*/, bool /*is_skip*/) {
    let skip = rec
        .err
        .as_deref()
        .is_some_and(|e| e.starts_with("skip:"));
    let err = rec.http >= 500
        || rec.err.as_deref().is_some_and(|e| !e.starts_with("skip:"));
    (err, skip)
}

fn error_class(rec: &OpRecord) -> ErrorClass {
    if rec.http == 0 {
        return ErrorClass::Transport;
    }
    match rec.http / 100 {
        5 => ErrorClass::Server,
        4 => ErrorClass::Client,
        _ => ErrorClass::Ok,
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ErrorClass {
    Ok,
    Client,
    Server,
    Transport,
}

// ============================================================================
// Timestamp helpers
// ============================================================================

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn bucket_start(ts: DateTime<Utc>, bucket_secs: u64) -> i64 {
    let unix = ts.timestamp();
    let step = bucket_secs as i64;
    unix - unix.rem_euclid(step)
}

fn fmt_ts(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map_or_else(|| format!("t={secs}"), |d| d.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn fmt_short_ts(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map_or_else(|| format!("t={secs}"), |d| d.format("%H:%M").to_string())
}

// ============================================================================
// Ops stream reader → Report
// ============================================================================

fn build_report(run_dir: &Path, bucket: Duration) -> Result<Report> {
    let config = read_json(&run_dir.join("config.json"))
        .context("read config.json")?;
    let summary = read_json_optional(&run_dir.join("summary.json"))
        .unwrap_or(serde_json::Value::Null);

    let bucket_seconds = bucket.as_secs().max(1);
    let mut r = Report {
        run_dir: run_dir.to_path_buf(),
        config,
        summary,
        per_op: BTreeMap::new(),
        total_ops: 0,
        total_errors: 0,
        total_skipped: 0,
        per_worker: BTreeMap::new(),
        top_error_rows: Vec::new(),
        top_transport_msgs: Vec::new(),
        bucket,
        bucket_seconds,
        ops_timeline: BTreeMap::new(),
        per_op_latency_timeline: BTreeMap::new(),
        stats_timeline: BTreeMap::new(),
        prom_timeline: BTreeMap::new(),
        health_events: Vec::new(),
        t_min: None,
        t_max: None,
    };

    // ---- ops.jsonl (streaming) --------------------------------------------
    let mut error_counts: HashMap<(String, String, u16), u64> = HashMap::new();
    let mut transport_msgs: HashMap<String, u64> = HashMap::new();
    let ops_path = run_dir.join("ops.jsonl");
    if ops_path.exists() {
        let f = File::open(&ops_path).with_context(|| format!("open {}", ops_path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: OpRecord = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let (is_err, is_skip) = classify_error(&rec);
            r.total_ops += 1;
            if is_err {
                r.total_errors += 1;
            }
            if is_skip {
                r.total_skipped += 1;
            }
            r.per_op
                .entry(rec.op.clone())
                .or_default()
                .push(&rec, is_err, is_skip);
            let w = r.per_worker.entry(rec.worker).or_default();
            w.n += 1;
            if is_err {
                w.err += 1;
            }
            if is_err {
                let target = truncate_target(&rec.target);
                let key = (rec.op.clone(), target, rec.http);
                *error_counts.entry(key).or_default() += 1;
                if rec.http == 0 {
                    if let Some(msg) = &rec.err {
                        // Normalise noisy URLs/timestamps out of the message.
                        let norm = normalise_transport_msg(msg);
                        *transport_msgs.entry(norm).or_default() += 1;
                    }
                }
            }
            if let Some(ts) = parse_ts(&rec.t) {
                r.t_min = Some(r.t_min.map_or(ts, |cur| cur.min(ts)));
                r.t_max = Some(r.t_max.map_or(ts, |cur| cur.max(ts)));
                let bkt = bucket_start(ts, bucket_seconds);
                let cell = r.ops_timeline.entry(bkt).or_default();
                cell.total_ops += 1;
                match error_class(&rec) {
                    ErrorClass::Ok => cell.ok_ops += 1,
                    ErrorClass::Client => {
                        // 404 on a doomed GET is normal churn, not an error.
                        if rec.http != 404 || rec.op != "get_random" {
                            cell.client_errors += 1;
                        }
                    }
                    ErrorClass::Server => cell.server_errors += 1,
                    ErrorClass::Transport => cell.transport_errors += 1,
                }
                if is_hot_op(&rec.op) {
                    let per_op = r.per_op_latency_timeline.entry(rec.op.clone()).or_default();
                    let vec = per_op.entry(bkt).or_default();
                    if vec.len() < 1024 {
                        vec.push(rec.ms);
                    }
                }
            }
        }
    }
    // Top errors: sort & keep 20.
    let mut error_rows: Vec<TopErrorRow> = error_counts
        .into_iter()
        .map(|((op, target, http), count)| TopErrorRow { op, target, http, count })
        .collect();
    error_rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.op.cmp(&b.op)));
    error_rows.truncate(20);
    r.top_error_rows = error_rows;

    let mut transport_rows: Vec<(String, u64)> = transport_msgs.into_iter().collect();
    transport_rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    transport_rows.truncate(10);
    r.top_transport_msgs = transport_rows;

    // ---- metrics.jsonl (streaming) ----------------------------------------
    let metrics_path = run_dir.join("metrics.jsonl");
    if metrics_path.exists() {
        let f = File::open(&metrics_path)
            .with_context(|| format!("open {}", metrics_path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(snap) = serde_json::from_str::<MetricsSnapshot>(&line) else {
                continue;
            };
            let Some(ts) = parse_ts(&snap.t) else { continue };
            let bkt = bucket_start(ts, bucket_seconds);
            if let Some(stats) = snap.stats.as_ref() {
                let s = parse_stats(stats);
                // Keep the LATEST snapshot per bucket (they're monotonic).
                r.stats_timeline.insert(bkt, s);
            }
            if !snap.prom.is_empty() {
                if let Some(p) = parse_prom(&snap.prom) {
                    r.prom_timeline.insert(bkt, p);
                }
            }
        }
    }

    // ---- health-events.jsonl ----------------------------------------------
    let health_path = run_dir.join("health-events.jsonl");
    if health_path.exists() {
        let f = File::open(&health_path)
            .with_context(|| format!("open {}", health_path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(env) = serde_json::from_str::<HealthEnvelope>(&line) else {
                continue;
            };
            let Some(ts) = parse_ts(&env.t) else { continue };
            // Trim SSE prefix.
            let payload = env
                .raw
                .strip_prefix("data:")
                .map_or(env.raw.as_str(), str::trim);
            r.health_events.push((ts, payload.to_string()));
        }
    }

    Ok(r)
}

fn truncate_target(t: &str) -> String {
    if t.len() > 80 {
        format!("{}…", &t[..79])
    } else {
        t.to_string()
    }
}

fn normalise_transport_msg(msg: &str) -> String {
    // Strip URL fragments; keep the reqwest error head.
    let head = msg.split(" for url (").next().unwrap_or(msg);
    head.chars().take(120).collect()
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn read_json_optional(path: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

fn parse_stats(v: &serde_json::Value) -> StatsSnapshot {
    fn u(v: &serde_json::Value, k: &str) -> u64 {
        v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0)
    }
    StatsSnapshot {
        nodes_live: u(v, "nodes_live"),
        nodes_total: u(v, "nodes_total"),
        objects_total: u(v, "objects_total"),
        shards_total: u(v, "shards_total"),
        shards_unique: u(v, "shards_unique"),
        bytes_total: u(v, "bytes_total"),
        auto_repairs_total: u(v, "auto_repairs_total"),
        auto_repair_failures_total: u(v, "auto_repair_failures_total"),
        scrub_runs_total: u(v, "scrub_runs_total"),
        scrub_repairs_total: u(v, "scrub_repairs_total"),
    }
}

fn parse_prom(text: &str) -> Option<PromSnapshot> {
    let scrape = Scrape::parse(text.lines().map(|l| Ok(l.to_string()))).ok()?;
    let mut out = PromSnapshot::default();
    for s in scrape.samples {
        let value_u = match s.value {
            PromValue::Counter(v) | PromValue::Gauge(v) | PromValue::Untyped(v) => v as i64,
            _ => continue,
        };
        let vu = value_u.max(0) as u64;
        match s.metric.as_str() {
            "holofs_backpressure_rejected_total" => out.backpressure_rejected_total += vu,
            "holofs_handler_timeouts_total" => out.handler_timeouts_total += vu,
            "holofs_catalog_persist_failures_total" => out.catalog_persist_failures_total += vu,
            "holofs_supervised_task_restarts_total" => out.supervised_task_restarts_total += vu,
            "holofs_admin_auth_failures_total" => out.admin_auth_failures_total += vu,
            "holofs_rate_limit_rejected_total" => out.rate_limit_rejected_total += vu,
            "holofs_backpressure_permits_available" => {
                if s.labels.get("bucket") == Some("medium") {
                    out.backpressure_permits_available_medium = value_u;
                } else if s.labels.get("bucket") == Some("long") {
                    out.backpressure_permits_available_long = value_u;
                }
            }
            _ => {}
        }
    }
    Some(out)
}

// ============================================================================
// SVG chart helpers
// ============================================================================

const SVG_W: usize = 720;
const SVG_H: usize = 200;
const PAD_L: usize = 60;
const PAD_R: usize = 20;
const PAD_T: usize = 15;
const PAD_B: usize = 30;

fn svg_line_chart(
    title: &str,
    xs: &[i64],
    series: &[(&str, &[f64], &str)], // label, values (same len as xs), colour
) -> String {
    if xs.is_empty() {
        return format!(
            r##"<div class="chart empty"><h4>{title}</h4><em>no data</em></div>"##
        );
    }
    let x_min = *xs.iter().min().unwrap();
    let x_max = *xs.iter().max().unwrap();
    let x_span = (x_max - x_min).max(1) as f64;
    let mut y_max: f64 = 0.0;
    for (_, ys, _) in series {
        for &v in *ys {
            if v.is_finite() && v > y_max {
                y_max = v;
            }
        }
    }
    if y_max <= 0.0 {
        y_max = 1.0;
    }
    let plot_w = (SVG_W - PAD_L - PAD_R) as f64;
    let plot_h = (SVG_H - PAD_T - PAD_B) as f64;
    let px = |x: i64| PAD_L as f64 + ((x - x_min) as f64 / x_span) * plot_w;
    let py = |y: f64| PAD_T as f64 + (1.0 - (y / y_max).clamp(0.0, 1.0)) * plot_h;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SVG_W} {SVG_H}" role="img" aria-label="{title}">"##
    );
    let _ = writeln!(
        svg,
        r##"<rect x="0" y="0" width="{SVG_W}" height="{SVG_H}" fill="#fafafa" stroke="#ddd"/>"##
    );
    // Y axis gridlines / labels (5 steps).
    for i in 0..=4 {
        let frac = i as f64 / 4.0;
        let y = PAD_T as f64 + (1.0 - frac) * plot_h;
        let val = y_max * frac;
        let _ = writeln!(
            svg,
            r##"<line x1="{PAD_L}" y1="{y:.1}" x2="{}" y2="{y:.1}" stroke="#eee"/>"##,
            SVG_W - PAD_R
        );
        let _ = writeln!(
            svg,
            r##"<text x="{}" y="{:.1}" font-size="10" text-anchor="end" fill="#666">{}</text>"##,
            PAD_L - 4,
            y + 3.0,
            fmt_num(val)
        );
    }
    // X axis labels (start, mid, end).
    for &frac in &[0.0f64, 0.5, 1.0] {
        let x = PAD_L as f64 + frac * plot_w;
        let ts = x_min + (frac * x_span) as i64;
        let _ = writeln!(
            svg,
            r##"<text x="{:.1}" y="{}" font-size="10" text-anchor="middle" fill="#666">{}</text>"##,
            x,
            SVG_H - 10,
            fmt_short_ts(ts)
        );
    }
    // Data.
    for (label, ys, colour) in series {
        let mut d = String::new();
        for (i, &y) in ys.iter().enumerate() {
            if !y.is_finite() {
                continue;
            }
            let cmd = if d.is_empty() { 'M' } else { 'L' };
            let _ = write!(d, " {cmd} {:.1} {:.1}", px(xs[i]), py(y));
        }
        let _ = writeln!(
            svg,
            r##"<path d="{d}" fill="none" stroke="{colour}" stroke-width="1.6" aria-label="{label}"/>"##
        );
    }
    // Legend.
    let mut legend_x = PAD_L as f64;
    for (label, _, colour) in series {
        let _ = writeln!(
            svg,
            r##"<circle cx="{legend_x:.1}" cy="12" r="4" fill="{colour}"/>"##
        );
        let _ = writeln!(
            svg,
            r##"<text x="{:.1}" y="15" font-size="11" fill="#333">{label}</text>"##,
            legend_x + 8.0
        );
        legend_x += (label.len() as f64 * 6.5) + 22.0;
    }
    let _ = writeln!(svg, "</svg>");
    format!(
        r##"<div class="chart"><h4>{title}</h4>{svg}</div>"##
    )
}

fn svg_stacked_errors(title: &str, xs: &[i64], layers: &[(&str, Vec<f64>, &str)]) -> String {
    if xs.is_empty() || layers.iter().all(|(_, v, _)| v.iter().all(|&x| x == 0.0)) {
        return format!(
            r##"<div class="chart empty"><h4>{title}</h4><em>no errors</em></div>"##
        );
    }
    let x_min = *xs.iter().min().unwrap();
    let x_max = *xs.iter().max().unwrap();
    let x_span = (x_max - x_min).max(1) as f64;
    let mut tallies = vec![0.0f64; xs.len()];
    for (_, v, _) in layers {
        for (i, &x) in v.iter().enumerate() {
            tallies[i] += x;
        }
    }
    let y_max = tallies.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let plot_w = (SVG_W - PAD_L - PAD_R) as f64;
    let plot_h = (SVG_H - PAD_T - PAD_B) as f64;
    let bar_w = (plot_w / xs.len() as f64).max(2.0) - 1.0;
    let px = |x: i64| PAD_L as f64 + ((x - x_min) as f64 / x_span) * plot_w;
    let py = |y: f64| PAD_T as f64 + (1.0 - (y / y_max).clamp(0.0, 1.0)) * plot_h;

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SVG_W} {SVG_H}" role="img" aria-label="{title}">"##
    );
    let _ = writeln!(
        svg,
        r##"<rect x="0" y="0" width="{SVG_W}" height="{SVG_H}" fill="#fafafa" stroke="#ddd"/>"##
    );
    // Y grid.
    for i in 0..=4 {
        let frac = i as f64 / 4.0;
        let y = PAD_T as f64 + (1.0 - frac) * plot_h;
        let _ = writeln!(
            svg,
            r##"<line x1="{PAD_L}" y1="{y:.1}" x2="{}" y2="{y:.1}" stroke="#eee"/>"##,
            SVG_W - PAD_R
        );
        let _ = writeln!(
            svg,
            r##"<text x="{}" y="{:.1}" font-size="10" text-anchor="end" fill="#666">{}</text>"##,
            PAD_L - 4,
            y + 3.0,
            fmt_num(y_max * frac)
        );
    }
    // X labels.
    for &frac in &[0.0f64, 0.5, 1.0] {
        let x = PAD_L as f64 + frac * plot_w;
        let ts = x_min + (frac * x_span) as i64;
        let _ = writeln!(
            svg,
            r##"<text x="{:.1}" y="{}" font-size="10" text-anchor="middle" fill="#666">{}</text>"##,
            x,
            SVG_H - 10,
            fmt_short_ts(ts)
        );
    }
    // Stacked bars.
    let mut cum = vec![0.0f64; xs.len()];
    for (label, values, colour) in layers {
        for (i, &v) in values.iter().enumerate() {
            if v <= 0.0 {
                continue;
            }
            let y0 = py(cum[i]);
            let y1 = py(cum[i] + v);
            let x = px(xs[i]) - bar_w / 2.0;
            let _ = writeln!(
                svg,
                r##"<rect x="{x:.1}" y="{y1:.1}" width="{bar_w:.1}" height="{h:.1}" fill="{colour}" aria-label="{label}"/>"##,
                h = (y0 - y1).max(0.0)
            );
            cum[i] += v;
        }
    }
    // Legend.
    let mut legend_x = PAD_L as f64;
    for (label, _, colour) in layers {
        let _ = writeln!(
            svg,
            r##"<rect x="{legend_x:.1}" y="8" width="10" height="8" fill="{colour}"/>"##
        );
        let _ = writeln!(
            svg,
            r##"<text x="{:.1}" y="15" font-size="11" fill="#333">{label}</text>"##,
            legend_x + 14.0
        );
        legend_x += (label.len() as f64 * 6.5) + 26.0;
    }
    let _ = writeln!(svg, "</svg>");
    format!(r##"<div class="chart"><h4>{title}</h4>{svg}</div>"##)
}

fn svg_bars(title: &str, labels: &[String], values: &[f64], colour: &str) -> String {
    if labels.is_empty() {
        return format!(
            r##"<div class="chart empty"><h4>{title}</h4><em>no data</em></div>"##
        );
    }
    let max_v = values.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let plot_w = (SVG_W - PAD_L - PAD_R) as f64;
    let plot_h = (SVG_H - PAD_T - PAD_B) as f64;
    let bar_slot = plot_w / labels.len() as f64;
    let bar_w = (bar_slot * 0.7).max(2.0);
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SVG_W} {SVG_H}" role="img" aria-label="{title}">"##
    );
    let _ = writeln!(
        svg,
        r##"<rect x="0" y="0" width="{SVG_W}" height="{SVG_H}" fill="#fafafa" stroke="#ddd"/>"##
    );
    for (i, &v) in values.iter().enumerate() {
        let frac = (v / max_v).clamp(0.0, 1.0);
        let h = frac * plot_h;
        let x = PAD_L as f64 + bar_slot * i as f64 + (bar_slot - bar_w) / 2.0;
        let y = PAD_T as f64 + plot_h - h;
        let _ = writeln!(
            svg,
            r##"<rect x="{x:.1}" y="{y:.1}" width="{bar_w:.1}" height="{h:.1}" fill="{colour}"/>"##
        );
        // X label.
        let cx = x + bar_w / 2.0;
        let _ = writeln!(
            svg,
            r##"<text x="{cx:.1}" y="{}" font-size="9" text-anchor="middle" fill="#666">{}</text>"##,
            SVG_H - 12,
            labels[i]
        );
    }
    for i in 0..=4 {
        let frac = i as f64 / 4.0;
        let y = PAD_T as f64 + (1.0 - frac) * plot_h;
        let _ = writeln!(
            svg,
            r##"<text x="{}" y="{y:.1}" font-size="10" text-anchor="end" fill="#666">{}</text>"##,
            PAD_L - 4,
            fmt_num(max_v * frac)
        );
    }
    let _ = writeln!(svg, "</svg>");
    format!(r##"<div class="chart"><h4>{title}</h4>{svg}</div>"##)
}

// ============================================================================
// Rendering: HTML
// ============================================================================

fn render_html(r: &mut Report) -> String {
    let mut out = String::new();
    let cfg = &r.config;
    let run_id = cfg.get("run_id").and_then(|v| v.as_str()).unwrap_or("<unknown>");
    let topology = cfg.get("topology").and_then(|v| v.as_str()).unwrap_or("external");
    let workers = cfg.get("workers").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let duration_ms = cfg.get("duration_ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let seed = cfg.get("seed").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let base = cfg.get("base").and_then(|v| v.as_str()).unwrap_or("");
    let version = cfg.get("binary_version").and_then(|v| v.as_str()).unwrap_or("?");

    let overall_error_rate = if r.total_ops == 0 {
        0.0
    } else {
        100.0 * r.total_errors as f64 / r.total_ops as f64
    };
    let elapsed_secs = r
        .t_max
        .zip(r.t_min)
        .map_or(0.0, |(hi, lo)| (hi - lo).num_seconds() as f64);
    let avg_rps = if elapsed_secs > 0.0 {
        r.total_ops as f64 / elapsed_secs
    } else {
        0.0
    };

    let _ = write!(
        out,
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>holofs-soak — {run_id}</title>
<style>
  :root {{ font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
  body {{ max-width: 1100px; margin: 24px auto; padding: 0 20px; color: #222; }}
  h1, h2, h3 {{ line-height: 1.2; }}
  h1 {{ margin-bottom: 4px; }}
  .lede {{ color: #666; margin: 0 0 24px; }}
  .cards {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 12px; margin: 16px 0 24px; }}
  .card {{ padding: 14px; border: 1px solid #e2e2e2; border-radius: 8px; background: #fff; }}
  .card .k {{ color: #666; font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; }}
  .card .v {{ font-size: 22px; font-weight: 600; margin-top: 4px; }}
  .card.warn .v {{ color: #b34700; }}
  .card.err .v {{ color: #b00020; }}
  table {{ border-collapse: collapse; width: 100%; margin: 6px 0 24px; }}
  th, td {{ padding: 6px 10px; text-align: left; border-bottom: 1px solid #eee; }}
  th {{ font-weight: 600; color: #555; background: #f7f7f7; }}
  td.num {{ text-align: right; font-variant-numeric: tabular-nums; }}
  code, pre {{ font: 12px/1.4 SFMono-Regular, Consolas, "Liberation Mono", Menlo, monospace; }}
  pre {{ background: #f7f7f7; padding: 10px 12px; border-radius: 6px; overflow-x: auto; }}
  .chart {{ margin: 6px 0 22px; }}
  .chart h4 {{ margin: 0 0 6px; color: #444; font-size: 13px; }}
  .chart.empty {{ padding: 12px; background: #f9f9f9; border-radius: 6px; color: #999; }}
  .grid2 {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(340px, 1fr)); gap: 16px; }}
  footer {{ margin-top: 40px; color: #999; font-size: 12px; text-align: center; }}
</style>
</head><body>
<h1>holofs-soak — {run_id}</h1>
<p class="lede">Topology <strong>{topology}</strong>, {workers} workers, seed <code>{seed}</code>, target <code>{base}</code>, runner v{version}.</p>
"##
    );

    // Overview cards.
    let _ = write!(
        out,
        r##"<div class="cards">
<div class="card"><div class="k">Total ops</div><div class="v">{}</div></div>
<div class="card {err_cls}"><div class="k">Errors</div><div class="v">{} ({:.2}%)</div></div>
<div class="card"><div class="k">Avg RPS</div><div class="v">{:.1}</div></div>
<div class="card"><div class="k">Elapsed</div><div class="v">{}</div></div>
<div class="card"><div class="k">Bucket</div><div class="v">{}</div></div>
<div class="card"><div class="k">Skipped</div><div class="v">{}</div></div>
</div>
"##,
        fmt_num(r.total_ops as f64),
        r.total_errors,
        overall_error_rate,
        avg_rps,
        fmt_hms(elapsed_secs as u64),
        fmt_hms(r.bucket_seconds),
        r.total_skipped,
        err_cls = if r.total_errors == 0 {
            ""
        } else if overall_error_rate >= 5.0 {
            "err"
        } else {
            "warn"
        }
    );
    let _ = write!(
        out,
        r##"<p style="color:#666;font-size:12px;margin-top:-8px;">Configured duration: {}. Skipped ops are worker retries against an empty catalog (`skip:no_files_yet` etc.) — not real failures.</p>"##,
        fmt_hms((duration_ms / 1000) as u64)
    );

    // Timings.
    out.push_str("<h2>Timings per operation</h2>\n");
    let mut per_op_rows: Vec<(String, u64, u64, u64, u64, u64, u64, u64)> = Vec::new();
    for (op, agg) in &mut r.per_op {
        let n = agg.n;
        let err = agg.err;
        let skip = agg.skipped;
        let p50 = agg.percentile(0.50);
        let p95 = agg.percentile(0.95);
        let p99 = agg.percentile(0.99);
        let mx = agg.max_ms();
        per_op_rows.push((op.clone(), n, err, skip, p50, p95, p99, mx));
    }
    per_op_rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    out.push_str(
        r##"<table><thead><tr><th>Op</th><th class="num">Count</th><th class="num">Errors</th><th class="num">Skipped</th><th class="num">p50 ms</th><th class="num">p95 ms</th><th class="num">p99 ms</th><th class="num">Max ms</th></tr></thead><tbody>"##,
    );
    for (op, n, err, skip, p50, p95, p99, mx) in &per_op_rows {
        let _ = write!(
            out,
            "<tr><td>{op}</td><td class=\"num\">{n}</td><td class=\"num\">{err}</td><td class=\"num\">{skip}</td><td class=\"num\">{p50}</td><td class=\"num\">{p95}</td><td class=\"num\">{p99}</td><td class=\"num\">{mx}</td></tr>"
        );
    }
    out.push_str("</tbody></table>\n");

    // Timelines: RPS + top 3 op latency.
    let bkts: Vec<i64> = r.ops_timeline.keys().copied().collect();
    let rps: Vec<f64> = bkts
        .iter()
        .map(|b| {
            let cell = r.ops_timeline.get(b).unwrap();
            cell.total_ops as f64 / r.bucket_seconds as f64
        })
        .collect();
    out.push_str("<h2>Throughput and error timelines</h2>\n<div class=\"grid2\">\n");
    out.push_str(&svg_line_chart(
        "Requests per second",
        &bkts,
        &[("rps", &rps, "#1f77b4")],
    ));
    // Error timeline (stacked).
    let client: Vec<f64> = bkts.iter().map(|b| r.ops_timeline[b].client_errors as f64).collect();
    let server: Vec<f64> = bkts.iter().map(|b| r.ops_timeline[b].server_errors as f64).collect();
    let transport: Vec<f64> = bkts.iter().map(|b| r.ops_timeline[b].transport_errors as f64).collect();
    out.push_str(&svg_stacked_errors(
        "Errors per bucket (stacked)",
        &bkts,
        &[
            ("4xx", client, "#f4a259"),
            ("5xx", server, "#d62728"),
            ("transport", transport, "#9467bd"),
        ],
    ));
    out.push_str("</div>\n");

    // Per-op p95 timeline (top 3 by count).
    let mut latency_series: Vec<(String, Vec<f64>, String)> = Vec::new();
    const OP_COLOURS: &[&str] = &["#1f77b4", "#2ca02c", "#ff7f0e", "#e377c2", "#8c564b"];
    for (i, (op, _, _, _, _, _, _, _)) in per_op_rows.iter().take(5).enumerate() {
        let mut ys = Vec::with_capacity(bkts.len());
        for &b in &bkts {
            let samples = r
                .per_op_latency_timeline
                .get(op)
                .and_then(|m| m.get(&b))
                .cloned()
                .unwrap_or_default();
            if samples.is_empty() {
                ys.push(f64::NAN);
                continue;
            }
            let mut s = samples;
            s.sort_unstable();
            let idx = ((s.len() as f64 - 1.0) * 0.95).round() as usize;
            ys.push(s[idx] as f64);
        }
        latency_series.push((op.clone(), ys, OP_COLOURS[i % OP_COLOURS.len()].to_string()));
    }
    let refs: Vec<(&str, &[f64], &str)> = latency_series
        .iter()
        .map(|(op, ys, c)| (op.as_str(), ys.as_slice(), c.as_str()))
        .collect();
    out.push_str(&svg_line_chart("p95 latency (ms) — top 5 ops", &bkts, &refs));

    // Per-worker rollup.
    out.push_str("<h2>Per-worker load</h2>\n");
    let mut worker_ids: Vec<usize> = r.per_worker.keys().copied().collect();
    worker_ids.sort_unstable();
    let labels: Vec<String> = worker_ids.iter().map(|i| format!("w{i:02}")).collect();
    let ns: Vec<f64> = worker_ids.iter().map(|w| r.per_worker[w].n as f64).collect();
    let ers: Vec<f64> = worker_ids.iter().map(|w| r.per_worker[w].err as f64).collect();
    out.push_str("<div class=\"grid2\">\n");
    out.push_str(&svg_bars("Ops per worker", &labels, &ns, "#1f77b4"));
    out.push_str(&svg_bars("Errors per worker", &labels, &ers, "#d62728"));
    out.push_str("</div>\n");

    // Top errors.
    out.push_str("<h2>Top errors</h2>\n");
    if r.top_error_rows.is_empty() {
        out.push_str("<p><em>No 5xx/4xx/transport errors recorded.</em></p>\n");
    } else {
        out.push_str(
            r##"<table><thead><tr><th>Op</th><th>Target</th><th class="num">HTTP</th><th class="num">Count</th></tr></thead><tbody>"##,
        );
        for row in &r.top_error_rows {
            let http_disp = if row.http == 0 { "transport".to_string() } else { row.http.to_string() };
            let _ = write!(
                out,
                "<tr><td>{}</td><td><code>{}</code></td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                row.op, html_escape(&row.target), http_disp, row.count
            );
        }
        out.push_str("</tbody></table>\n");
    }

    if !r.top_transport_msgs.is_empty() {
        out.push_str("<h3>Top transport-level error messages</h3>\n");
        out.push_str(r##"<table><thead><tr><th>Message</th><th class="num">Count</th></tr></thead><tbody>"##);
        for (msg, n) in &r.top_transport_msgs {
            let _ = write!(
                out,
                "<tr><td><code>{}</code></td><td class=\"num\">{n}</td></tr>",
                html_escape(msg)
            );
        }
        out.push_str("</tbody></table>\n");
    }

    // Cluster telemetry.
    out.push_str("<h2>Cluster telemetry</h2>\n");
    let stats_bkts: Vec<i64> = r.stats_timeline.keys().copied().collect();
    if stats_bkts.is_empty() {
        out.push_str("<p><em>No `/api/stats` snapshots captured (metrics.jsonl was empty).</em></p>\n");
    } else {
        let objects: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].objects_total as f64)
            .collect();
        let shards: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].shards_total as f64)
            .collect();
        let bytes_mb: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].bytes_total as f64 / 1_048_576.0)
            .collect();
        let auto_rep: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].auto_repairs_total as f64)
            .collect();
        let scrub_rep: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].scrub_repairs_total as f64)
            .collect();
        let nodes_live: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].nodes_live as f64)
            .collect();
        out.push_str("<div class=\"grid2\">\n");
        out.push_str(&svg_line_chart(
            "Catalog size",
            &stats_bkts,
            &[
                ("objects", &objects, "#1f77b4"),
                ("shards", &shards, "#2ca02c"),
            ],
        ));
        out.push_str(&svg_line_chart(
            "Bytes stored (MiB)",
            &stats_bkts,
            &[("bytes_total", &bytes_mb, "#ff7f0e")],
        ));
        out.push_str(&svg_line_chart(
            "Repair counters (cumulative)",
            &stats_bkts,
            &[
                ("auto_repairs", &auto_rep, "#d62728"),
                ("scrub_repairs", &scrub_rep, "#9467bd"),
            ],
        ));
        out.push_str(&svg_line_chart(
            "Live nodes",
            &stats_bkts,
            &[("nodes_live", &nodes_live, "#17becf")],
        ));
        out.push_str("</div>\n");
    }
    // Prometheus timeline.
    let prom_bkts: Vec<i64> = r.prom_timeline.keys().copied().collect();
    if !prom_bkts.is_empty() {
        let backpressure: Vec<f64> = prom_bkts
            .iter()
            .map(|b| r.prom_timeline[b].backpressure_rejected_total as f64)
            .collect();
        let timeouts: Vec<f64> = prom_bkts
            .iter()
            .map(|b| r.prom_timeline[b].handler_timeouts_total as f64)
            .collect();
        let rate_lim: Vec<f64> = prom_bkts
            .iter()
            .map(|b| r.prom_timeline[b].rate_limit_rejected_total as f64)
            .collect();
        let permits_med: Vec<f64> = prom_bkts
            .iter()
            .map(|b| r.prom_timeline[b].backpressure_permits_available_medium as f64)
            .collect();
        let permits_long: Vec<f64> = prom_bkts
            .iter()
            .map(|b| r.prom_timeline[b].backpressure_permits_available_long as f64)
            .collect();
        out.push_str("<div class=\"grid2\">\n");
        out.push_str(&svg_line_chart(
            "Server-side rejections (cumulative)",
            &prom_bkts,
            &[
                ("backpressure", &backpressure, "#d62728"),
                ("timeouts", &timeouts, "#8c564b"),
                ("rate_limit", &rate_lim, "#e377c2"),
            ],
        ));
        out.push_str(&svg_line_chart(
            "Backpressure permits available",
            &prom_bkts,
            &[
                ("medium", &permits_med, "#1f77b4"),
                ("long", &permits_long, "#2ca02c"),
            ],
        ));
        out.push_str("</div>\n");
    }

    // Health events.
    out.push_str("<h2>Health-events sample</h2>\n");
    if r.health_events.is_empty() {
        out.push_str("<p><em>No SSE frames received.</em></p>\n");
    } else {
        let sample: Vec<&(DateTime<Utc>, String)> = if r.health_events.len() <= 40 {
            r.health_events.iter().collect()
        } else {
            let head = r.health_events.iter().take(20);
            let tail = r.health_events.iter().rev().take(20).collect::<Vec<_>>();
            head.chain(tail.into_iter().rev()).collect()
        };
        out.push_str("<pre>");
        for (ts, msg) in &sample {
            let _ = write!(out, "{}  {}\n", ts.format("%H:%M:%S"), html_escape(msg));
        }
        out.push_str("</pre>\n");
        if r.health_events.len() > 40 {
            let _ = write!(
                out,
                "<p style=\"color:#666\">{} more events elided.</p>",
                r.health_events.len() - 40
            );
        }
    }

    // Reproducibility.
    out.push_str("<h2>Reproducibility</h2>\n<pre>");
    let _ = write!(
        out,
        "run dir:   {}\nconfig.json: {}\nsummary.json: {}\n",
        r.run_dir.display(),
        r.run_dir.join("config.json").display(),
        r.run_dir.join("summary.json").display()
    );
    if let Ok(cfg_str) = serde_json::to_string_pretty(cfg) {
        out.push_str("\n");
        out.push_str(&html_escape(&cfg_str));
    }
    out.push_str("</pre>\n");
    out.push_str("<footer>Generated by holofs-soak-report.</footer>\n</body></html>");
    out
}

fn fmt_num(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 10_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else if v >= 1_000.0 {
        format!("{:.2}k", v / 1_000.0)
    } else if v >= 10.0 {
        format!("{v:.0}")
    } else if v > 0.0 {
        format!("{v:.2}")
    } else {
        "0".to_string()
    }
}

fn fmt_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

// ============================================================================
// Rendering: Markdown
// ============================================================================

const SPARK_BARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline(vals: &[f64]) -> String {
    if vals.is_empty() {
        return String::new();
    }
    let max = vals.iter().copied().fold(0.0f64, f64::max);
    if max <= 0.0 {
        return "▁".repeat(vals.len());
    }
    let mut out = String::new();
    for &v in vals {
        let idx = ((v / max) * (SPARK_BARS.len() - 1) as f64).round() as usize;
        out.push(SPARK_BARS[idx.min(SPARK_BARS.len() - 1)]);
    }
    out
}

fn render_md(r: &mut Report) -> String {
    let cfg = &r.config;
    let run_id = cfg.get("run_id").and_then(|v| v.as_str()).unwrap_or("<unknown>");
    let topology = cfg.get("topology").and_then(|v| v.as_str()).unwrap_or("external");
    let workers = cfg.get("workers").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let duration_ms = cfg.get("duration_ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let seed = cfg.get("seed").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let base = cfg.get("base").and_then(|v| v.as_str()).unwrap_or("");

    let overall_error_rate = if r.total_ops == 0 {
        0.0
    } else {
        100.0 * r.total_errors as f64 / r.total_ops as f64
    };
    let elapsed_secs = r
        .t_max
        .zip(r.t_min)
        .map_or(0.0, |(hi, lo)| (hi - lo).num_seconds() as f64);
    let avg_rps = if elapsed_secs > 0.0 {
        r.total_ops as f64 / elapsed_secs
    } else {
        0.0
    };

    let mut out = String::new();
    let _ = writeln!(out, "## holofs-soak — {run_id}");
    let _ = writeln!(
        out,
        "\nTopology **{topology}**, {workers} workers, seed `{seed}`, target `{base}`.\n"
    );
    let _ = writeln!(out, "## Overview\n");
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| Total ops | {} |", r.total_ops);
    let _ = writeln!(
        out,
        "| Errors | {} ({overall_error_rate:.2}%) |",
        r.total_errors
    );
    let _ = writeln!(out, "| Skipped | {} |", r.total_skipped);
    let _ = writeln!(out, "| Avg RPS | {avg_rps:.1} |");
    let _ = writeln!(out, "| Elapsed | {} |", fmt_hms(elapsed_secs as u64));
    let _ = writeln!(
        out,
        "| Configured duration | {} |",
        fmt_hms((duration_ms / 1000) as u64)
    );
    let _ = writeln!(out, "| Bucket | {} |", fmt_hms(r.bucket_seconds));
    let _ = writeln!(out);

    // Timings.
    let _ = writeln!(out, "## Timings per operation\n");
    let _ = writeln!(
        out,
        "| Op | Count | Errors | Skipped | p50 ms | p95 ms | p99 ms | Max ms |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|");
    let mut per_op_rows: Vec<(String, u64, u64, u64, u64, u64, u64, u64)> = Vec::new();
    for (op, agg) in &mut r.per_op {
        per_op_rows.push((
            op.clone(),
            agg.n,
            agg.err,
            agg.skipped,
            agg.percentile(0.50),
            agg.percentile(0.95),
            agg.percentile(0.99),
            agg.max_ms(),
        ));
    }
    per_op_rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (op, n, err, skip, p50, p95, p99, mx) in &per_op_rows {
        let _ = writeln!(
            out,
            "| {op} | {n} | {err} | {skip} | {p50} | {p95} | {p99} | {mx} |"
        );
    }
    let _ = writeln!(out);

    // Timelines via sparkline.
    let bkts: Vec<i64> = r.ops_timeline.keys().copied().collect();
    if !bkts.is_empty() {
        let rps: Vec<f64> = bkts
            .iter()
            .map(|b| r.ops_timeline[b].total_ops as f64 / r.bucket_seconds as f64)
            .collect();
        let errs: Vec<f64> = bkts
            .iter()
            .map(|b| {
                let c = &r.ops_timeline[b];
                (c.server_errors + c.client_errors + c.transport_errors) as f64
            })
            .collect();
        let _ = writeln!(out, "## Timeline (bucket = {})\n", fmt_hms(r.bucket_seconds));
        let _ = writeln!(
            out,
            "- RPS:      `{}`  ({} → {})",
            sparkline(&rps),
            fmt_short_ts(*bkts.first().unwrap()),
            fmt_short_ts(*bkts.last().unwrap())
        );
        let _ = writeln!(
            out,
            "- Errors/bucket: `{}` (max: {:.0})\n",
            sparkline(&errs),
            errs.iter().copied().fold(0.0, f64::max)
        );
    }

    // Per-worker (top 10 by load and top 5 by errors).
    let mut wk: Vec<(usize, u64, u64)> = r
        .per_worker
        .iter()
        .map(|(&w, r)| (w, r.n, r.err))
        .collect();
    if !wk.is_empty() {
        wk.sort_by_key(|r| std::cmp::Reverse(r.1));
        let _ = writeln!(out, "## Per-worker load (top 10)\n");
        let _ = writeln!(out, "| Worker | Ops | Errors |");
        let _ = writeln!(out, "|---|---:|---:|");
        for (w, n, e) in wk.iter().take(10) {
            let _ = writeln!(out, "| w{w:02} | {n} | {e} |");
        }
        let _ = writeln!(out);
    }

    // Top errors.
    let _ = writeln!(out, "## Top errors\n");
    if r.top_error_rows.is_empty() {
        let _ = writeln!(out, "_None recorded._\n");
    } else {
        let _ = writeln!(out, "| Op | Target | HTTP | Count |");
        let _ = writeln!(out, "|---|---|---:|---:|");
        for row in &r.top_error_rows {
            let http_disp = if row.http == 0 { "transport".into() } else { row.http.to_string() };
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} |",
                row.op, row.target, http_disp, row.count
            );
        }
        let _ = writeln!(out);
    }
    if !r.top_transport_msgs.is_empty() {
        let _ = writeln!(out, "### Top transport messages\n");
        let _ = writeln!(out, "| Message | Count |");
        let _ = writeln!(out, "|---|---:|");
        for (msg, n) in &r.top_transport_msgs {
            let _ = writeln!(out, "| `{msg}` | {n} |");
        }
        let _ = writeln!(out);
    }

    // Cluster telemetry.
    let stats_bkts: Vec<i64> = r.stats_timeline.keys().copied().collect();
    if !stats_bkts.is_empty() {
        let objects: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].objects_total as f64)
            .collect();
        let shards: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].shards_total as f64)
            .collect();
        let bytes_mb: Vec<f64> = stats_bkts
            .iter()
            .map(|b| r.stats_timeline[b].bytes_total as f64 / 1_048_576.0)
            .collect();
        let _ = writeln!(out, "## Cluster telemetry\n");
        let _ = writeln!(
            out,
            "- objects_total: `{}` (last: {:.0})",
            sparkline(&objects),
            objects.last().copied().unwrap_or(0.0)
        );
        let _ = writeln!(
            out,
            "- shards_total:  `{}` (last: {:.0})",
            sparkline(&shards),
            shards.last().copied().unwrap_or(0.0)
        );
        let _ = writeln!(
            out,
            "- bytes_total:   `{}` (last: {:.1} MiB)",
            sparkline(&bytes_mb),
            bytes_mb.last().copied().unwrap_or(0.0)
        );
        let auto_rep_final = r
            .stats_timeline
            .values()
            .last()
            .map_or(0, |s| s.auto_repairs_total);
        let scrub_rep_final = r
            .stats_timeline
            .values()
            .last()
            .map_or(0, |s| s.scrub_repairs_total);
        let auto_fail_final = r
            .stats_timeline
            .values()
            .last()
            .map_or(0, |s| s.auto_repair_failures_total);
        let _ = writeln!(
            out,
            "- auto_repairs_total (final): {auto_rep_final} (failures: {auto_fail_final})"
        );
        let _ = writeln!(out, "- scrub_repairs_total (final): {scrub_rep_final}");
        let _ = writeln!(out);
    }
    // Prometheus final counters.
    if let Some((_, p)) = r.prom_timeline.iter().next_back() {
        let _ = writeln!(out, "### Prometheus counters (final snapshot)\n");
        let _ = writeln!(out, "| Metric | Value |");
        let _ = writeln!(out, "|---|---:|");
        let _ = writeln!(out, "| backpressure_rejected_total | {} |", p.backpressure_rejected_total);
        let _ = writeln!(out, "| handler_timeouts_total | {} |", p.handler_timeouts_total);
        let _ = writeln!(out, "| catalog_persist_failures_total | {} |", p.catalog_persist_failures_total);
        let _ = writeln!(out, "| supervised_task_restarts_total | {} |", p.supervised_task_restarts_total);
        let _ = writeln!(out, "| admin_auth_failures_total | {} |", p.admin_auth_failures_total);
        let _ = writeln!(out, "| rate_limit_rejected_total | {} |", p.rate_limit_rejected_total);
        let _ = writeln!(out, "| backpressure_permits_available{{medium}} | {} |", p.backpressure_permits_available_medium);
        let _ = writeln!(out, "| backpressure_permits_available{{long}} | {} |", p.backpressure_permits_available_long);
        let _ = writeln!(out);
    }

    // Health events sample.
    if !r.health_events.is_empty() {
        let _ = writeln!(out, "## Health events sample\n");
        let take_n = 20.min(r.health_events.len());
        let _ = writeln!(out, "```");
        for (ts, msg) in r.health_events.iter().take(take_n) {
            let _ = writeln!(out, "{}  {msg}", ts.format("%H:%M:%S"));
        }
        if r.health_events.len() > take_n {
            let _ = writeln!(
                out,
                "… {} more events elided ({} total)",
                r.health_events.len() - take_n,
                r.health_events.len()
            );
        }
        let _ = writeln!(out, "```\n");
    }
    let _ = writeln!(out, "## Reproducibility\n");
    let _ = writeln!(out, "- Run directory: `{}`", r.run_dir.display());
    if let Ok(cfg_str) = serde_json::to_string_pretty(cfg) {
        let _ = writeln!(out, "\n```json\n{cfg_str}\n```");
    }
    out
}

// ============================================================================
// Run-dir discovery
// ============================================================================

fn most_recent_run(root: &Path) -> Result<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    let iter = std::fs::read_dir(root)
        .with_context(|| format!("read {}", root.display()))?;
    for entry in iter {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Runner names dirs as UTC timestamps `YYYY-MM-DDT…`.
        if !name.starts_with(char::is_numeric) {
            continue;
        }
        match &best {
            None => best = Some((name.clone(), path.clone())),
            Some((cur, _)) if &name > cur => best = Some((name.clone(), path.clone())),
            _ => {}
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no run directories under {}", root.display()))
}

// ============================================================================
// main
// ============================================================================

fn main() -> Result<()> {
    let cli = Cli::parse();
    let run_dir = match cli.run {
        Some(p) => p,
        None => most_recent_run(&cli.root)?,
    };
    if !run_dir.is_dir() {
        bail!("run dir {} is not a directory", run_dir.display());
    }
    let bucket = parse_duration(&cli.bucket)?;
    let mut report = build_report(&run_dir, bucket)?;
    let outputs = match cli.format {
        Format::Html => vec![("html", render_html(&mut report))],
        Format::Md => vec![("md", render_md(&mut report))],
        Format::Both => vec![
            ("html", render_html(&mut report)),
            ("md", render_md(&mut report)),
        ],
    };
    for (ext, body) in outputs {
        let path = match (&cli.output, &cli.format) {
            (Some(p), Format::Both) => {
                // Attach extension to the caller's stem, so a single --output
                // still splits into two files.
                p.with_extension(ext)
            }
            (Some(p), _) => p.clone(),
            (None, _) => run_dir.join(format!("report.{ext}")),
        };
        std::fs::write(&path, body)
            .with_context(|| format!("write {}", path.display()))?;
        eprintln!("[report] wrote {}", path.display());
    }
    Ok(())
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_to_max() {
        // 0 → ▁ (idx 0), 5 → 4/7 ≈ 0.57·7 = 4 → ▅, 10 → █.
        assert_eq!(sparkline(&[0.0, 5.0, 10.0]), "▁▅█");
    }

    #[test]
    fn duration_parser_shares_units() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn bucket_start_aligns() {
        let ts = Utc.timestamp_opt(1_700_000_123, 0).single().unwrap();
        assert_eq!(bucket_start(ts, 300), 1_700_000_100);
    }

    #[test]
    fn classify_skip_vs_error() {
        let rec = OpRecord {
            t: "2020-01-01T00:00:00Z".into(),
            worker: 0,
            op: "get_random".into(),
            target: String::new(),
            http: 0,
            ms: 0,
            err: Some("skip:no_files_yet".into()),
        };
        let (e, s) = classify_error(&rec);
        assert!(!e && s);
    }

    #[test]
    fn classify_transport_is_error() {
        let rec = OpRecord {
            t: "2020-01-01T00:00:00Z".into(),
            worker: 0,
            op: "put_new".into(),
            target: String::new(),
            http: 0,
            ms: 0,
            err: Some("error sending request".into()),
        };
        let (e, s) = classify_error(&rec);
        assert!(e && !s);
    }
}
