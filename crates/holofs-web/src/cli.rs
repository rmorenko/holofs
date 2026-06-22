//! Command-line parser for the `holofs-web` binary.
//!
//! Every flag has an env-var fallback (clap `env = "..."`) so existing
//! deployments that drive the binary through env vars keep working. Flags
//! take precedence over env vars. See `--help` for the full surface.

#![cfg(feature = "ssr")]

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::bootstrap::BootstrapConfig;
use holofs_storage::identity::PUBKEY_LEN;

/// Output format for tracing-subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable single-line text (ANSI-coloured on TTY).
    Text,
    /// Newline-delimited JSON, one event per line — ship to journald, ELK, etc.
    Json,
}

/// Parsed CLI options. Builds the per-subsystem configs (bootstrap, address,
/// tracing) so `main` stays a thin glue layer.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "holofs-web",
    version,
    about = "axum + Leptos SSR frontend for a holofs cluster",
    long_about = "Boots a holofs cluster (embedded nodes or distributed via signed whitelist), \
serves the HTTP gateway on --addr (default 127.0.0.1:8787) and an optional Prometheus \
endpoint on --metrics-listen. All flags have HOLOFS_* env-var fallbacks."
)]
pub struct Cli {
    /// HTTP listen address (`host:port`).
    #[arg(long, env = "LEPTOS_SITE_ADDR", default_value = "127.0.0.1:8787")]
    pub addr: SocketAddr,

    /// Root directory for persistent storage (shards under `node_NN`, catalog
    /// at `<storage>/catalog.bin` unless `--catalog` overrides).
    #[arg(long, env = "HOLOFS_STORAGE_DIR", default_value = "./holofs-data")]
    pub storage: PathBuf,

    /// Explicit catalog file path.
    #[arg(long, env = "HOLOFS_CATALOG")]
    pub catalog: Option<PathBuf>,

    /// Signed admin whitelist — switches the binary into distributed mode.
    #[arg(long, env = "HOLOFS_WHITELIST")]
    pub whitelist: Option<PathBuf>,

    /// 64-char hex admin pubkey required to verify `--whitelist`.
    #[arg(long, env = "HOLOFS_ADMIN_PUBKEY", value_parser = parse_pubkey_hex)]
    pub admin_pubkey: Option<[u8; PUBKEY_LEN]>,

    /// Optional image to seed `photo.png` on the first boot.
    #[arg(long, env = "HOLOFS_SEED_PHOTO")]
    pub seed_photo: Option<PathBuf>,

    /// Do not create demo objects even when the catalog is empty.
    #[arg(long, env = "HOLOFS_NO_SEED")]
    pub no_seed: bool,

    /// Optional Prometheus endpoint listen address. If unset, `/metrics` is
    /// only served on the main HTTP port (no separate listener).
    #[arg(long, env = "HOLOFS_METRICS_LISTEN")]
    pub metrics_listen: Option<SocketAddr>,

    /// `tracing` filter spec (e.g. `info`, `info,holofs_core=debug`).
    #[arg(long, env = "HOLOFS_LOG", default_value = "info,holofs_web=debug")]
    pub log: String,

    /// `text` (default, ANSI-coloured) or `json` (one event per line).
    #[arg(long, env = "HOLOFS_LOG_FORMAT", value_enum, default_value = "text")]
    pub log_format: LogFormat,

    /// Encrypt the wire protocol with rustls TLS (gateway↔nodes). In
    /// embedded mode the binary auto-generates a self-signed CA + per-node
    /// leaf certs under `<storage>/certs/`. In distributed mode you must
    /// also pass `--tls-cert`, `--tls-key`, and `--tls-ca-cert`.
    #[arg(long, env = "HOLOFS_TLS")]
    pub tls: bool,

    /// Require + verify a client certificate signed by the same CA on every
    /// node connection. Implies `--tls`.
    #[arg(long, env = "HOLOFS_MTLS")]
    pub mtls: bool,

    /// PEM file with the gateway/node leaf certificate (distributed TLS only).
    #[arg(long, env = "HOLOFS_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// PEM file with the matching private key (distributed TLS only).
    #[arg(long, env = "HOLOFS_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// PEM file with the trust-root CA cert (distributed TLS only).
    #[arg(long, env = "HOLOFS_TLS_CA_CERT")]
    pub tls_ca_cert: Option<PathBuf>,
}

impl Cli {
    /// Project the CLI into a [`BootstrapConfig`] consumed by
    /// `bootstrap_cluster`. `--mtls` implies `--tls`.
    pub fn bootstrap_config(&self) -> BootstrapConfig {
        BootstrapConfig {
            storage: self.storage.clone(),
            catalog: self.catalog.clone(),
            whitelist: self.whitelist.clone(),
            admin_pubkey: self.admin_pubkey,
            seed_photo: self.seed_photo.clone(),
            no_seed: self.no_seed,
            tls: crate::bootstrap::TlsOptions {
                enabled: self.tls || self.mtls,
                mtls: self.mtls,
                cert_path: self.tls_cert.clone(),
                key_path: self.tls_key.clone(),
                ca_path: self.tls_ca_cert.clone(),
            },
        }
    }
}

fn parse_pubkey_hex(s: &str) -> Result<[u8; PUBKEY_LEN], String> {
    if s.len() != PUBKEY_LEN * 2 {
        return Err(format!(
            "admin pubkey must be {} hex chars",
            PUBKEY_LEN * 2
        ));
    }
    let mut out = [0u8; PUBKEY_LEN];
    for i in 0..PUBKEY_LEN {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("bad hex at byte {i}: {e}"))?;
    }
    Ok(out)
}
