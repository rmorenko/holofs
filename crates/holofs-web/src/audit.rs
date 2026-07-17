//! Admin-action audit log (P1.5).
//!
//! Append-only JSONL file with one line per admin action:
//!
//! ```json
//! {"ts_unix_ms":1720000000000,"actor":"admin_token","verb":"drain_node",
//!  "target":"idx=10","result":"ok","details":{"drained_objects":12345,
//!  "failed":0,"purged":true}}
//! ```
//!
//! Design notes:
//! - **Best-effort.** IO failures on the audit path do NOT fail the
//!   admin action itself — a full disk shouldn't 500 the operator's
//!   drain call. Errors log to stderr instead.
//! - **JSON Lines.** One record per line, no framing — trivial to
//!   `tail -f`, `jq -c`, `logrotate`. Rotation is externally
//!   configured; the logger holds the file open across rotations
//!   (matches journald / rsyslog norms — pointing `HOLOFS_AUDIT_LOG`
//!   at a new path + gateway restart cleanly transitions).
//! - **Handlers-only.** Only `/admin/*` HTTP handlers call
//!   `AuditLogger::log`. Wire-level RPCs on the node side stay
//!   out of scope — the audit target is gateway operator actions,
//!   not data-plane traffic.
//! - **Actor field.** Best-effort attribution. Today all admin
//!   handlers gate on the same shared admin token, so `actor` is
//!   always `"admin_token"`. When we wire per-user auth (see the
//!   scenario-B backlog) actor becomes meaningful.
//!
//! Off switch: `HOLOFS_AUDIT_LOG=off` disables logging entirely (the
//! shared handle becomes a no-op). Any other value is treated as a
//! filesystem path; unset falls back to `<storage>/audit.log`.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide singleton set by bootstrap. Handlers reach it via
/// [`log_event`] rather than threading an `Extension<Arc<AuditLogger>>`
/// through every axum handler signature. Uninitialised = every event
/// silently discarded (in-memory `Gateway::new` tests, e.g.).
static GLOBAL: OnceLock<Arc<AuditLogger>> = OnceLock::new();

/// Install the process-wide audit logger. Called once at gateway
/// bootstrap after opening the on-disk file. Idempotent — subsequent
/// calls do nothing (mirrors `OnceLock` semantics; a re-init would
/// leak the previous file handle).
pub fn set_global(logger: Arc<AuditLogger>) {
    let _ = GLOBAL.set(logger);
}

/// Handler-friendly entry point. Silently no-ops if
/// [`set_global`] was never called (in-memory tests, missing
/// bootstrap) — the intent is "audit if the operator turned it on;
/// don't fail otherwise".
pub fn log_event(event: &AuditEvent) {
    if let Some(l) = GLOBAL.get() {
        l.log(event);
    }
}

/// One admin event. Serialised to JSON on the audit path; the
/// concrete field set is stable across releases so downstream
/// tooling (SIEM ingest, `jq` pipelines) can rely on it.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Millisecond Unix timestamp. Emitted server-side so ordering
    /// is consistent even across timezone-quirky clients.
    pub ts_unix_ms: u64,
    /// Best-effort attribution. Today: `"admin_token"` for every
    /// admin handler (they all gate on the same bearer). Left
    /// as a string so per-user attribution can slot in without a
    /// schema bump.
    pub actor: String,
    /// The action taken — matches the HTTP handler name
    /// (`drain_node`, `add_node`, `toggle_node`, `gc_orphans`,
    /// `catalog_names`, …).
    pub verb: &'static str,
    /// Free-form target descriptor. Convention: `"key=value"` or a
    /// short human-readable phrase. Empty when the verb has no
    /// natural target.
    pub target: String,
    /// `"ok"` or `"error"` — coarse outcome for grep-friendliness.
    /// Details go under `details`.
    pub result: &'static str,
    /// Raw JSON fragment appended under a `"details"` key. Handlers
    /// build this with the same string-format they use for their
    /// HTTP response body so the two stay in lock-step.
    pub details_json: Option<String>,
}

impl AuditEvent {
    /// Constructor with the timestamp filled in from wall clock.
    /// Actor defaults to `"admin_token"` (the current authn scope).
    pub fn now(verb: &'static str, target: impl Into<String>, result: &'static str) -> Self {
        Self {
            ts_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            actor: "admin_token".to_string(),
            verb,
            target: target.into(),
            result,
            details_json: None,
        }
    }

    /// Attach a pre-serialised JSON blob under the `details` key.
    /// Callers typically build this alongside their HTTP response so
    /// both surfaces carry identical structure.
    pub fn with_details(mut self, details_json: impl Into<String>) -> Self {
        self.details_json = Some(details_json.into());
        self
    }

    /// Serialise to a single JSON line (trailing '\n' included).
    /// Handrolled to avoid pulling `serde_derive` for a single
    /// struct; the field set is small and stable.
    pub fn to_jsonl(&self) -> String {
        let mut buf = String::with_capacity(128);
        buf.push_str(r#"{"ts_unix_ms":"#);
        buf.push_str(&self.ts_unix_ms.to_string());
        buf.push_str(r#","actor":""#);
        json_escape_into(&self.actor, &mut buf);
        buf.push_str(r#"","verb":""#);
        buf.push_str(self.verb);
        buf.push_str(r#"","target":""#);
        json_escape_into(&self.target, &mut buf);
        buf.push_str(r#"","result":""#);
        buf.push_str(self.result);
        buf.push('"');
        if let Some(d) = &self.details_json {
            buf.push_str(r#","details":"#);
            buf.push_str(d);
        }
        buf.push('}');
        buf.push('\n');
        buf
    }
}

/// Process-wide audit logger. Wrapped in `Arc` and shared into
/// axum extensions so every handler can call it without threading
/// the handle through the world.
pub struct AuditLogger {
    inner: Option<Mutex<BufWriter<File>>>,
    /// Cached path for diagnostics — `None` when disabled.
    path: Option<PathBuf>,
}

impl AuditLogger {
    /// Open (append) the audit log at `<storage>/audit.log`, honouring
    /// the `HOLOFS_AUDIT_LOG` env override. Reads the env var
    /// exactly once; the per-value semantics live in the pure
    /// [`resolve_audit_log_path`] helper (isolated so tests don't
    /// have to touch process env state).
    ///
    /// Failure to open the file downgrades to a disabled logger AND
    /// prints an eprintln! — surfaces at boot but never blocks it.
    pub fn open(storage_dir: &Path) -> Arc<Self> {
        let env_val = std::env::var("HOLOFS_AUDIT_LOG").ok();
        Self::open_at(resolve_audit_log_path(storage_dir, env_val.as_deref()))
    }

    /// Path-first constructor. `None` → disabled logger; `Some(p)` →
    /// try to open for append. Extracted so tests can exercise
    /// every branch without racing on the process-wide env var.
    pub fn open_at(path: Option<PathBuf>) -> Arc<Self> {
        let Some(path) = path else {
            return Arc::new(Self {
                inner: None,
                path: None,
            });
        };
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => Arc::new(Self {
                inner: Some(Mutex::new(BufWriter::new(f))),
                path: Some(path),
            }),
            Err(e) => {
                eprintln!(
                    "audit: could not open {} for append: {e} — audit logging disabled",
                    path.display()
                );
                Arc::new(Self {
                    inner: None,
                    path: None,
                })
            }
        }
    }
}

/// Pure resolver: given the storage dir and the `HOLOFS_AUDIT_LOG`
/// env value (if any), return the on-disk audit log path or `None`
/// for a disabled logger.
///
/// - `env_val = None` → `Some(storage_dir/audit.log)`.
/// - `env_val = Some("off")` (case-insensitive) → `None`.
/// - `env_val = Some(path)` → `Some(PathBuf::from(path))`.
pub fn resolve_audit_log_path(storage_dir: &Path, env_val: Option<&str>) -> Option<PathBuf> {
    match env_val {
        None => Some(storage_dir.join("audit.log")),
        Some(v) if v.eq_ignore_ascii_case("off") => None,
        Some(v) => Some(PathBuf::from(v)),
    }
}

// Second impl block for methods post the pure helper — keeps the
// docs above co-located with `open`.
impl AuditLogger {

    /// Construct a permanently-disabled logger. Used by unit tests
    /// and by the in-memory `Gateway::new` path where there's no
    /// storage dir to write to.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            inner: None,
            path: None,
        })
    }

    /// On-disk path if the logger is active; `None` when disabled.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the logger will actually persist events.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Append one event. Best-effort: IO failure is logged to
    /// stderr but never propagated — the caller (an admin handler)
    /// must not fail because the audit path is broken.
    pub fn log(&self, event: &AuditEvent) {
        let Some(mtx) = &self.inner else {
            return;
        };
        let line = event.to_jsonl();
        let mut w = match mtx.lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!("audit: log mutex poisoned, skipping event verb={}", event.verb);
                return;
            }
        };
        if let Err(e) = w.write_all(line.as_bytes()) {
            eprintln!("audit: write failed ({e}), skipping event verb={}", event.verb);
            return;
        }
        // `flush` is intentional per event so a crash doesn't lose
        // the last handful of admin actions — the audit log's whole
        // point is post-incident forensics.
        if let Err(e) = w.flush() {
            eprintln!("audit: flush failed ({e}), event may be delayed");
        }
    }
}

/// Minimal JSON string escape — handles the seven characters
/// mandatory in `RFC 8259 §7`. Avoids pulling `serde_json` just for
/// the audit path (the details fragment is already JSON, and the
/// three string fields we escape here — `actor`, `verb`, `target` —
/// are entirely ASCII in the current handler set).
fn json_escape_into(s: &str, buf: &mut String) {
    for c in s.chars() {
        match c {
            '"' => buf.push_str(r#"\""#),
            '\\' => buf.push_str(r#"\\"#),
            '\n' => buf.push_str(r"\n"),
            '\r' => buf.push_str(r"\r"),
            '\t' => buf.push_str(r"\t"),
            '\x08' => buf.push_str(r"\b"),
            '\x0c' => buf.push_str(r"\f"),
            c if (c as u32) < 0x20 => {
                buf.push_str(&format!(r"\u{:04x}", c as u32));
            }
            c => buf.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn audit_event_to_jsonl_shape() {
        let ev = AuditEvent {
            ts_unix_ms: 1_720_000_000_000,
            actor: "admin_token".into(),
            verb: "drain_node",
            target: "idx=10".into(),
            result: "ok",
            details_json: Some(r#"{"drained":42,"failed":0}"#.into()),
        };
        let line = ev.to_jsonl();
        assert!(line.ends_with('\n'));
        assert!(line.contains(r#""ts_unix_ms":1720000000000"#));
        assert!(line.contains(r#""verb":"drain_node""#));
        assert!(line.contains(r#""target":"idx=10""#));
        assert!(line.contains(r#""result":"ok""#));
        assert!(line.contains(r#""details":{"drained":42,"failed":0}"#));
        // Round-trip through a real JSON parser — proves we emit
        // valid JSON, not just "looks right" strings.
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["verb"], "drain_node");
        assert_eq!(parsed["details"]["drained"], 42);
    }

    #[test]
    fn audit_event_escapes_quotes_and_backslashes_in_target() {
        let ev = AuditEvent::now("test", r#"path="a\b\c""#, "ok");
        let line = ev.to_jsonl();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["target"], r#"path="a\b\c""#);
    }

    // === path resolver (pure function) ================================
    // Cover every branch of the env-value contract WITHOUT touching
    // process env state — race-free even under `cargo test`'s parallel
    // runner.

    #[test]
    fn resolve_defaults_to_storage_audit_log_when_env_absent() {
        let dir = TempDir::new().unwrap();
        let p = resolve_audit_log_path(dir.path(), None);
        assert_eq!(p, Some(dir.path().join("audit.log")));
    }

    #[test]
    fn resolve_returns_none_when_env_is_off() {
        let dir = TempDir::new().unwrap();
        assert_eq!(resolve_audit_log_path(dir.path(), Some("off")), None);
        // Case-insensitive.
        assert_eq!(resolve_audit_log_path(dir.path(), Some("OFF")), None);
        assert_eq!(resolve_audit_log_path(dir.path(), Some("Off")), None);
    }

    #[test]
    fn resolve_takes_env_path_verbatim() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("elsewhere.log");
        let p = resolve_audit_log_path(dir.path(), Some(target.to_str().unwrap()));
        assert_eq!(p, Some(target));
    }

    // === logger IO (path-first constructor, no env mutation) ==========

    #[test]
    fn logger_disabled_when_path_is_none() {
        let log = AuditLogger::open_at(None);
        assert!(!log.is_enabled());
        assert!(log.path().is_none());
        // Must not panic even though it's a no-op.
        log.log(&AuditEvent::now("noop", "", "ok"));
    }

    #[test]
    fn logger_persists_lines_and_flushes_per_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLogger::open_at(Some(path.clone()));
        assert!(log.is_enabled());
        for i in 0..3 {
            log.log(&AuditEvent::now("test", format!("i={i}"), "ok"));
        }
        // Per-event flush → readable immediately without dropping
        // the logger.
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 lines, got {raw:?}");
        for (i, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["target"], format!("i={i}"));
        }
    }

    #[test]
    fn logger_open_survives_missing_parent_by_disabling() {
        // Path whose parent directory doesn't exist. Open must NOT
        // panic and must NOT propagate the error — instead returns
        // a disabled logger + an eprintln.
        let log = AuditLogger::open_at(Some(PathBuf::from("/no/such/dir/audit.log")));
        assert!(!log.is_enabled());
    }
}
