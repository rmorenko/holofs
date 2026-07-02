//! v0.7 — TOML configuration file support.
//!
//! Historically holofs-web took every knob via CLI flag or `HOLOFS_*`
//! env var. That's fine for one-off runs but noisy in production
//! deployments: a Helm chart or systemd unit ends up with a 25-line
//! `Environment=` block that has to be maintained everywhere the
//! daemon is deployed.
//!
//! This module adds a single-file alternative:
//! `--config /etc/holofs/holofs.toml` (or `HOLOFS_CONFIG=/path`).
//! The TOML mirrors the env-var vocabulary, grouped by concern.
//! Priority ladder (highest wins):
//!
//! 1. CLI flag (`--medium-concurrency 128`)
//! 2. Env var (`HOLOFS_MEDIUM_CONCURRENCY=128`)
//! 3. Config file (`[reliability] medium_concurrency = 128`)
//! 4. Compile-time default
//!
//! Implementation trick: the config file is loaded **before** clap
//! parses the CLI, and every field is translated into an env-var
//! `std::env::set_var` call — but only when the env var is not
//! already set. Clap's existing `#[arg(env = "...")]` machinery then
//! sees the enriched environment and applies its normal
//! CLI > env > default resolution. That way we didn't have to
//! rewrite the flag surface or teach clap about a new source.
//!
//! Field naming: TOML uses snake_case, env vars uppercase snake with
//! `HOLOFS_` prefix. Ex: `[reliability] scrub_interval_secs = 600`
//! becomes `HOLOFS_SCRUB_INTERVAL=600`. See [`ConfigFile::apply_to_env`]
//! for the full mapping.

#![cfg(feature = "ssr")]

use std::path::Path;

use serde::Deserialize;

/// Root TOML document. Every section + every field is optional so a
/// half-written config file is valid — the missing knobs fall back
/// to env vars / defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub cluster: ClusterSection,
    #[serde(default)]
    pub features: FeaturesSection,
    #[serde(default)]
    pub reliability: ReliabilitySection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default)]
    pub mcp: McpSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub addr: Option<String>,
    pub storage: Option<String>,
    pub catalog: Option<String>,
    pub log: Option<String>,
    pub log_format: Option<String>,
    pub metrics_listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    pub enabled: Option<bool>,
    pub mtls: Option<bool>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub ca_cert: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSection {
    pub whitelist: Option<String>,
    pub admin_pubkey: Option<String>,
    pub embed_base_port: Option<u16>,
    pub seed_photo: Option<String>,
    pub no_seed: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeaturesSection {
    pub embed: Option<bool>,
    pub versions: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReliabilitySection {
    pub medium_concurrency: Option<usize>,
    pub long_concurrency: Option<usize>,
    pub scrub_interval_secs: Option<u64>,
    pub reputation_persist_interval_secs: Option<u64>,
    pub monitor_interval_secs: Option<u64>,
    pub audit_interval_secs: Option<u64>,
    pub rpc_timeout_ms: Option<u64>,
    pub versions_keep_last: Option<usize>,
    /// v0.7 streaming-PUT max body size, in bytes.
    pub upload_max_size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminSection {
    /// Inline token — convenient but ends up in plaintext in the
    /// config file. Use [`Self::token_file`] for production.
    pub token: Option<String>,
    /// Path to a file whose first line is the admin token. Read
    /// once at bootstrap; permissions/ownership are the operator's
    /// responsibility.
    pub token_file: Option<String>,
    pub unauthenticated: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSection {
    pub token: Option<String>,
    pub token_file: Option<String>,
}

impl ConfigFile {
    /// Parse a TOML file at `path`. Returns `Ok(None)` when the file
    /// does not exist so the caller can treat "no config file" as
    /// the ordinary case. Any parse / IO error surfaces so a
    /// mistyped path or malformed TOML fails loud at boot instead
    /// of silently reverting to defaults.
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let parsed: Self = toml::from_str(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad TOML at {}: {e}", path.display()),
            )
        })?;
        Ok(Some(parsed))
    }

    /// Populate `HOLOFS_*` env vars from this config for any variable
    /// the process env has not already set. The final priority
    /// ladder that main.rs sees is:
    ///
    /// - CLI flag (clap handles it)
    /// - Env var already exported to the process
    /// - This config-file value (via `set_var`)
    /// - clap's compile-time default
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe on POSIX. This method
    /// must be called before spawning any thread that reads env
    /// vars — i.e. at the very top of `main.rs`, before the tokio
    /// runtime starts.
    pub fn apply_to_env(&self) {
        // Resolve admin/mcp token_file references first so
        // `HOLOFS_ADMIN_TOKEN` / `HOLOFS_MCP_TOKEN` become simple
        // string values downstream.
        let admin_token = resolve_secret(&self.admin.token, &self.admin.token_file);
        let mcp_token = resolve_secret(&self.mcp.token, &self.mcp.token_file);

        // server
        set_if_missing("LEPTOS_SITE_ADDR", self.server.addr.as_deref());
        set_if_missing("HOLOFS_STORAGE_DIR", self.server.storage.as_deref());
        set_if_missing("HOLOFS_CATALOG", self.server.catalog.as_deref());
        set_if_missing("HOLOFS_LOG", self.server.log.as_deref());
        set_if_missing("HOLOFS_LOG_FORMAT", self.server.log_format.as_deref());
        set_if_missing(
            "HOLOFS_METRICS_LISTEN",
            self.server.metrics_listen.as_deref(),
        );

        // tls
        set_bool_if_missing("HOLOFS_TLS", self.tls.enabled);
        set_bool_if_missing("HOLOFS_MTLS", self.tls.mtls);
        set_if_missing("HOLOFS_TLS_CERT", self.tls.cert.as_deref());
        set_if_missing("HOLOFS_TLS_KEY", self.tls.key.as_deref());
        set_if_missing("HOLOFS_TLS_CA_CERT", self.tls.ca_cert.as_deref());

        // cluster
        set_if_missing("HOLOFS_WHITELIST", self.cluster.whitelist.as_deref());
        set_if_missing("HOLOFS_ADMIN_PUBKEY", self.cluster.admin_pubkey.as_deref());
        if let Some(p) = self.cluster.embed_base_port {
            set_if_missing_string("HOLOFS_EMBED_BASE_PORT", &p.to_string());
        }
        set_if_missing("HOLOFS_SEED_PHOTO", self.cluster.seed_photo.as_deref());
        set_bool_if_missing("HOLOFS_NO_SEED", self.cluster.no_seed);

        // features
        set_bool_if_missing("HOLOFS_ENABLE_EMBED", self.features.embed);
        set_bool_if_missing("HOLOFS_ENABLE_VERSIONS", self.features.versions);

        // reliability
        if let Some(v) = self.reliability.medium_concurrency {
            set_if_missing_string("HOLOFS_MEDIUM_CONCURRENCY", &v.to_string());
        }
        if let Some(v) = self.reliability.long_concurrency {
            set_if_missing_string("HOLOFS_LONG_CONCURRENCY", &v.to_string());
        }
        if let Some(v) = self.reliability.scrub_interval_secs {
            set_if_missing_string("HOLOFS_SCRUB_INTERVAL", &v.to_string());
        }
        if let Some(v) = self.reliability.reputation_persist_interval_secs {
            set_if_missing_string("HOLOFS_REPUTATION_PERSIST_INTERVAL", &v.to_string());
        }
        if let Some(v) = self.reliability.monitor_interval_secs {
            set_if_missing_string("HOLOFS_MONITOR_INTERVAL", &v.to_string());
        }
        if let Some(v) = self.reliability.audit_interval_secs {
            set_if_missing_string("HOLOFS_AUDIT_INTERVAL", &v.to_string());
        }
        if let Some(v) = self.reliability.rpc_timeout_ms {
            set_if_missing_string("HOLOFS_RPC_TIMEOUT_MS", &v.to_string());
        }
        if let Some(v) = self.reliability.versions_keep_last {
            set_if_missing_string("HOLOFS_VERSIONS_KEEP_LAST", &v.to_string());
        }
        if let Some(v) = self.reliability.upload_max_size {
            set_if_missing_string("HOLOFS_UPLOAD_MAX_SIZE", &v.to_string());
        }

        // admin
        set_if_missing("HOLOFS_ADMIN_TOKEN", admin_token.as_deref());
        set_bool_if_missing("HOLOFS_ADMIN_UNAUTHENTICATED", self.admin.unauthenticated);

        // mcp
        set_if_missing("HOLOFS_MCP_TOKEN", mcp_token.as_deref());
    }
}

/// If `inline` is set use it directly; otherwise read the file at
/// `file_path` and return its trimmed first non-empty line. Errors
/// (missing file, bad UTF-8) → `None` with a WARN. Secrets are
/// deliberately not echoed to the log.
fn resolve_secret(inline: &Option<String>, file_path: &Option<String>) -> Option<String> {
    if let Some(v) = inline.as_deref() {
        return Some(v.to_string());
    }
    let Some(path) = file_path.as_deref() else {
        return None;
    };
    match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.trim().to_string()),
        Err(e) => {
            tracing::warn!(path, error = %e, "reading token_file failed");
            None
        }
    }
}

fn set_if_missing(key: &str, value: Option<&str>) {
    let Some(v) = value else {
        return;
    };
    if std::env::var_os(key).is_none() {
        // SAFETY: caller (main.rs) guarantees no other threads read
        // the env at this point.
        std::env::set_var(key, v);
    }
}

fn set_if_missing_string(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, value);
    }
}

fn set_bool_if_missing(key: &str, value: Option<bool>) {
    let Some(b) = value else {
        return;
    };
    if std::env::var_os(key).is_none() {
        // Match clap's boolean env-var convention: any non-empty
        // value is truthy, unset is false. We deliberately write
        // "true"/"false" strings rather than "1"/"0" so operators
        // reading their live env with `env | grep HOLOFS_` see
        // something legible.
        std::env::set_var(key, if b { "true" } else { "false" });
    }
}

/// Peek at the process command line and env for `--config <path>` /
/// `HOLOFS_CONFIG=<path>` before clap runs. Returns `None` when no
/// config file was requested.
///
/// The pre-scan is intentionally simple — it accepts `--config path`
/// and `--config=path` forms, nothing fancier. Any real error
/// (unrecognised flag, missing value) is left to clap to surface
/// downstream; we only care about extracting the path so
/// [`ConfigFile::apply_to_env`] can run before clap parses the flag
/// surface.
pub fn detect_config_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("HOLOFS_CONFIG") {
        if !p.is_empty() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(std::path::PathBuf::from);
        }
        if let Some(v) = a.strip_prefix("--config=") {
            return Some(std::path::PathBuf::from(v));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "holofs-config-{tag}-{}-{now}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_returns_ok_none() {
        let p = scratch_path("missing").join("nope.toml");
        assert!(ConfigFile::load(&p).unwrap().is_none());
    }

    #[test]
    fn parses_full_document() {
        let dir = scratch_path("parse");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("holofs.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
[server]
addr = "0.0.0.0:8080"
storage = "/data"
log_format = "json"

[tls]
enabled = true
mtls = false

[reliability]
medium_concurrency = 32
long_concurrency = 4
scrub_interval_secs = 300

[admin]
token = "secret-xyz"
"#
        )
        .unwrap();
        let cfg = ConfigFile::load(&path).unwrap().unwrap();
        assert_eq!(cfg.server.addr.as_deref(), Some("0.0.0.0:8080"));
        assert_eq!(cfg.server.storage.as_deref(), Some("/data"));
        assert_eq!(cfg.server.log_format.as_deref(), Some("json"));
        assert_eq!(cfg.tls.enabled, Some(true));
        assert_eq!(cfg.tls.mtls, Some(false));
        assert_eq!(cfg.reliability.medium_concurrency, Some(32));
        assert_eq!(cfg.reliability.long_concurrency, Some(4));
        assert_eq!(cfg.reliability.scrub_interval_secs, Some(300));
        assert_eq!(cfg.admin.token.as_deref(), Some("secret-xyz"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_field() {
        let dir = scratch_path("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("holofs.toml");
        std::fs::write(&path, "[server]\naddr = \"127.0.0.1:1\"\nblah = 42\n").unwrap();
        let err = ConfigFile::load(&path).unwrap_err();
        assert!(err.to_string().contains("blah"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_secret_prefers_inline_over_file() {
        // Even with a bogus file path, inline wins.
        let out = resolve_secret(
            &Some("inline-token".into()),
            &Some("/nonexistent/tokenfile".into()),
        );
        assert_eq!(out.as_deref(), Some("inline-token"));
    }

    #[test]
    fn resolve_secret_reads_token_file_first_nonempty_line() {
        let dir = scratch_path("secret");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "\n\n  the-token  \nother-stuff\n").unwrap();
        let out = resolve_secret(&None, &Some(path.to_string_lossy().into_owned()));
        assert_eq!(out.as_deref(), Some("the-token"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
