//! Single typed snapshot of every `HOLOFS_*` env knob the web layer
//! consults at startup or on a hot path. Per the S4-5 review finding —
//! prior to this file the same env var could be read from three
//! different places (`bootstrap.rs`, `main.rs`, a handler on the
//! request path) with subtly different fallbacks; a typo in one place
//! meant a real behaviour difference between the setup log and the
//! serve loop.
//!
//! Layering: the resolver runs *after* [`ConfigFile::apply_to_env`]
//! has populated the environment from `--config`, so a value from the
//! TOML file is visible here just like a direct env-var export. That
//! preserves the CLI > env > TOML > default priority chain the
//! `config_file` docstring advertises.
//!
//! Read pattern:
//!
//! ```ignore
//! use crate::runtime_config::RuntimeConfig;
//!
//! let cfg = RuntimeConfig::init(); // once at bootstrap
//! // ...later, anywhere in the crate:
//! let cap = RuntimeConfig::get().reliability.medium_concurrency;
//! ```
//!
//! `init()` is idempotent — `RuntimeConfig::get()` before `init()`
//! panics loudly rather than returning a half-baked default so a
//! caller reading the config in a wrong order surfaces fast.

use std::path::PathBuf;
use std::sync::OnceLock;

use holofs_gateway::{
    DEFAULT_ENCODE_CONCURRENCY, DEFAULT_ENCODE_QUEUE_MAX, DEFAULT_LONG_CONCURRENCY,
    DEFAULT_MEDIUM_CONCURRENCY,
};

/// Every runtime-visible `HOLOFS_*` knob the web layer touches, in a
/// single typed struct. Populated once at bootstrap via
/// [`RuntimeConfig::init`].
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub storage: StorageConfig,
    pub features: FeaturesConfig,
    pub reliability: ReliabilityConfig,
    pub admin: AdminConfig,
    pub mcp: McpConfig,
    pub security: SecurityConfig,
    pub embed_cluster: EmbedClusterConfig,
}

#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Storage root (shards, catalog, embeddings).
    pub storage_dir: PathBuf,
    /// Explicit catalog path override.
    pub catalog: Option<PathBuf>,
    /// Whitelist file for the multi-node topology.
    pub whitelist: Option<PathBuf>,
    /// Admin pubkey (hex) — required alongside a whitelist.
    pub admin_pubkey: Option<String>,
    /// A seed photo copied into the storage root on first boot.
    pub seed_photo: Option<PathBuf>,
    /// Explicit "do not seed on boot" gate.
    pub no_seed: bool,
    /// Max upload body size in bytes for handler streaming.
    pub upload_max_size: u64,
}

#[derive(Debug, Clone)]
pub struct FeaturesConfig {
    pub enable_embed: bool,
    pub enable_versions: bool,
    /// PUT hands off to a background encoder and returns 202 instead
    /// of blocking. Handler-hot read; snapshot'd here to avoid the
    /// per-request env lookup.
    pub async_encode: bool,
}

#[derive(Debug, Clone)]
pub struct ReliabilityConfig {
    pub medium_concurrency: usize,
    pub long_concurrency: usize,
    pub encode_concurrency: usize,
    pub encode_queue_max: usize,
    pub versions_keep_last: Option<usize>,
    pub monitor_interval_secs: u64,
    pub audit_interval_secs: u64,
    pub reputation_persist_interval_secs: u64,
    pub scrub_interval_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AdminConfig {
    pub token: Option<String>,
    pub unauthenticated_allowed: bool,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SecurityConfig {
    pub rate_limit_rps_per_ip: f64,
    pub rate_limit_burst: f64,
    pub rate_limit_idle_secs: u64,
}

#[derive(Debug, Clone)]
pub struct EmbedClusterConfig {
    pub base_port: u16,
}

static RUNTIME_CONFIG: OnceLock<RuntimeConfig> = OnceLock::new();

impl RuntimeConfig {
    /// Read env once and freeze into the process-wide singleton.
    /// Bootstrap calls this after `ConfigFile::apply_to_env` runs, so
    /// TOML defaults are visible.
    pub fn init() -> &'static RuntimeConfig {
        RUNTIME_CONFIG.get_or_init(Self::resolve)
    }

    /// Access the frozen config. Panics if [`Self::init`] hasn't run.
    pub fn get() -> &'static RuntimeConfig {
        RUNTIME_CONFIG
            .get()
            .expect("RuntimeConfig::init() must run at bootstrap before RuntimeConfig::get()")
    }

    fn resolve() -> RuntimeConfig {
        RuntimeConfig {
            storage: StorageConfig {
                storage_dir: std::env::var("HOLOFS_STORAGE_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("./holofs-data")),
                catalog: std::env::var("HOLOFS_CATALOG").ok().map(PathBuf::from),
                whitelist: std::env::var("HOLOFS_WHITELIST").ok().map(PathBuf::from),
                admin_pubkey: std::env::var("HOLOFS_ADMIN_PUBKEY")
                    .ok()
                    .filter(|s| !s.is_empty()),
                seed_photo: std::env::var("HOLOFS_SEED_PHOTO").ok().map(PathBuf::from),
                no_seed: parse_bool(std::env::var("HOLOFS_NO_SEED").ok().as_deref()),
                upload_max_size: std::env::var("HOLOFS_UPLOAD_MAX_SIZE")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200 * 1024 * 1024),
            },
            features: FeaturesConfig {
                enable_embed: parse_bool(std::env::var("HOLOFS_ENABLE_EMBED").ok().as_deref()),
                enable_versions: parse_bool(
                    std::env::var("HOLOFS_ENABLE_VERSIONS").ok().as_deref(),
                ),
                async_encode: parse_bool(std::env::var("HOLOFS_ASYNC_ENCODE").ok().as_deref()),
            },
            reliability: ReliabilityConfig {
                medium_concurrency: std::env::var("HOLOFS_MEDIUM_CONCURRENCY")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_MEDIUM_CONCURRENCY),
                long_concurrency: std::env::var("HOLOFS_LONG_CONCURRENCY")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_LONG_CONCURRENCY),
                encode_concurrency: std::env::var("HOLOFS_ENCODE_CONCURRENCY")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_ENCODE_CONCURRENCY),
                encode_queue_max: std::env::var("HOLOFS_ENCODE_QUEUE_MAX")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| {
                        std::env::var("HOLOFS_ENCODE_CONCURRENCY")
                            .ok()
                            .and_then(|s| s.parse::<usize>().ok())
                            .map(|c| c.saturating_mul(8).max(DEFAULT_ENCODE_QUEUE_MAX))
                            .unwrap_or(DEFAULT_ENCODE_QUEUE_MAX)
                    }),
                versions_keep_last: std::env::var("HOLOFS_VERSIONS_KEEP_LAST")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .filter(|&n: &usize| n > 0),
                monitor_interval_secs: std::env::var("HOLOFS_MONITOR_INTERVAL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(15),
                audit_interval_secs: std::env::var("HOLOFS_AUDIT_INTERVAL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(60),
                reputation_persist_interval_secs: std::env::var(
                    "HOLOFS_REPUTATION_PERSIST_INTERVAL",
                )
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
                scrub_interval_secs: std::env::var("HOLOFS_SCRUB_INTERVAL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(600),
            },
            admin: AdminConfig {
                token: std::env::var("HOLOFS_ADMIN_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty()),
                unauthenticated_allowed: parse_bool(
                    std::env::var("HOLOFS_ADMIN_UNAUTHENTICATED").ok().as_deref(),
                ),
            },
            mcp: McpConfig {
                token: std::env::var("HOLOFS_MCP_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty()),
            },
            security: SecurityConfig {
                rate_limit_rps_per_ip: std::env::var("HOLOFS_RATE_LIMIT_RPS_PER_IP")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                rate_limit_burst: std::env::var("HOLOFS_RATE_LIMIT_BURST")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(20.0),
                rate_limit_idle_secs: std::env::var("HOLOFS_RATE_LIMIT_IDLE_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300),
            },
            embed_cluster: EmbedClusterConfig {
                base_port: std::env::var("HOLOFS_EMBED_BASE_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(9200),
            },
        }
    }
}

fn parse_bool(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
