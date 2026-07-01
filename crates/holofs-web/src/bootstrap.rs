//! Cluster bootstrap for `holofs-web` — server-only.
//!
//! Mirrors the logic in `crates/holofs-cli/src/bin/holofs-http.rs::main`:
//! parse the same env vars and CLI flags, spawn embedded nodes (or load a
//! whitelist), prepare catalog, build `Gateway`, and start the health
//! monitor + PoR auditor. The result lives in a [`Bootstrap`] handle that
//! the axum binary keeps around for handler context.

#![cfg(feature = "ssr")]

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use holofs_client::put_object;
use holofs_cluster::audit::{self, AuditConfig, AuditEvent, AuditOutcome};
use holofs_cluster::monitor::{run_periodic, Event, MonitorConfig};
use holofs_cluster::reputation::Reputation;
use holofs_codec::image_io::{load_photo, synth};
use holofs_core::gf::Gf;
use holofs_core::hash::hex;
use holofs_core::transform::coeff_layer;
use holofs_core::{dims_from_env, K, LEVELS, NLAYERS, N_NODES, RED};
use holofs_gateway::{ClusterInfo, Gateway};
use holofs_model::fs::Directory;
use holofs_model::manifest::Manifest;
use holofs_model::placement::Placement;
use holofs_storage::identity::PUBKEY_LEN;
use holofs_storage::node_service::spawn_node_persistent_with_tls;
use holofs_storage::tls::TlsMaterial;
use holofs_storage::whitelist::Whitelist;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Bootstrap configuration. Defaults match `holofs-http` (CLI flags →
/// `BootstrapConfig` fields). Reading from env happens in [`Self::from_env`].
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Root directory for persistent storage. Embedded nodes live under
    /// `<storage>/node_NN`; catalog defaults to `<storage>/catalog.bin`.
    pub storage: PathBuf,
    /// Explicit catalog file path. Falls back to `<storage>/catalog.bin`.
    pub catalog: Option<PathBuf>,
    /// Optional path to a signed admin whitelist — enables distributed mode.
    pub whitelist: Option<PathBuf>,
    /// Required admin pubkey (trust anchor) when whitelist is set.
    pub admin_pubkey: Option<[u8; PUBKEY_LEN]>,
    /// Path to an image used to seed `photo.png` on first boot.
    pub seed_photo: Option<PathBuf>,
    /// Suppress demo-object seeding even when the catalog is empty.
    pub no_seed: bool,
    /// rustls TLS settings for the node↔gateway wire protocol.
    pub tls: TlsOptions,
    /// Stage 12.8: enable CLIP-based semantic search. Index lives at
    /// `<storage>/embeddings.bin`; first inference downloads ~155 MiB
    /// of model weights into `~/.cache/huggingface/hub`.
    pub enable_embed: bool,
    /// Stage 13.4: enable per-object version history. Side files under
    /// `<storage>/versions/<sanitized_name>/`; prior shards kept live
    /// on the cluster across PUTs.
    pub enable_versions: bool,
}

/// Where to source TLS material for the wire protocol.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Off (default) → plain TCP. On → encrypt + authenticate the wire.
    pub enabled: bool,
    /// Off → server TLS only. On → require + verify a client cert too.
    pub mtls: bool,
    /// Distributed mode: leaf cert / key / CA paths supplied externally.
    /// All three must be `Some` when set; embedded mode leaves them `None`
    /// and the bootstrap generates a fresh CA + leaves at boot.
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    pub ca_path: Option<PathBuf>,
}

impl BootstrapConfig {
    /// Build a config from environment variables, mirroring
    /// `holofs-http`'s `HOLOFS_*` env contract.
    pub fn from_env() -> Self {
        let storage = std::env::var("HOLOFS_STORAGE_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./holofs-data"));
        let catalog = std::env::var("HOLOFS_CATALOG").ok().map(PathBuf::from);
        let whitelist = std::env::var("HOLOFS_WHITELIST").ok().map(PathBuf::from);
        let admin_pubkey = std::env::var("HOLOFS_ADMIN_PUBKEY")
            .ok()
            .and_then(|s| parse_pubkey_hex(&s));
        let seed_photo = std::env::var("HOLOFS_SEED_PHOTO").ok().map(PathBuf::from);
        let no_seed = std::env::var("HOLOFS_NO_SEED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let enable_embed = std::env::var("HOLOFS_ENABLE_EMBED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let enable_versions = std::env::var("HOLOFS_ENABLE_VERSIONS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            storage,
            catalog,
            whitelist,
            admin_pubkey,
            seed_photo,
            no_seed,
            tls: TlsOptions::default(),
            enable_embed,
            enable_versions,
        }
    }
}

/// Handles produced by [`bootstrap_cluster`]. Held alive by the binary; if
/// dropped, the monitor and auditor tasks are cancelled.
pub struct Bootstrap {
    /// Configured gateway — the axum app provides this to handlers via
    /// `leptos_routes_with_context`.
    pub gateway: Arc<Gateway>,
    /// Background health monitor task.
    pub monitor: tokio::task::JoinHandle<()>,
    /// Background PoR auditor task.
    pub auditor: tokio::task::JoinHandle<()>,
    /// Background shard scrub task. `None` when the operator disabled it
    /// via `HOLOFS_SCRUB_INTERVAL=0` — the auto-repair-on-read path
    /// (inside `Gateway::decode_with_autorepair`) still runs.
    pub scrub: Option<tokio::task::JoinHandle<()>>,
    /// Embedded cluster node listener tasks. Empty when running in
    /// distributed / whitelist mode — the operator manages those nodes
    /// out-of-process. On shutdown these are `abort()`ed so the ports
    /// free up promptly; TLS/TCP connections in flight complete
    /// naturally on their own per-connection spawn.
    pub node_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// N1: cancellation signal shared with every background task
    /// spawned during bootstrap (monitor + auditor + scrub). Callers
    /// trigger a coordinated shutdown by calling `shutdown.cancel()`
    /// and then `await`ing the JoinHandles above.
    pub shutdown: CancellationToken,
}

/// Run the same boot sequence as `holofs-http`:
///
/// 1. Spawn embedded nodes (or read a whitelist for distributed mode).
/// 2. Load/seed the catalog.
/// 3. Build `Gateway::new_persistent`.
/// 4. Start `run_periodic` for the health monitor and the PoR auditor.
pub async fn bootstrap_cluster(
    config: &BootstrapConfig,
) -> Result<Bootstrap, Box<dyn std::error::Error>> {
    let gf = Arc::new(Gf::new());
    let (w, h) = dims_from_env();

    std::fs::create_dir_all(&config.storage)?;

    // Prepare TLS material before binding nodes — embedded mode generates a
    // fresh CA so every node can be issued a leaf signed by the same trust
    // root; distributed mode loads operator-supplied PEM files. The same
    // CA is later turned into a client config for the gateway's RPC path.
    let (node_server_cfg, gateway_client_cfg, mut node_signer) = build_tls(&config.tls)?;

    let shutdown = CancellationToken::new();
    let mut node_task_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let (node_addrs, zones, n_nodes) = if let Some(wl_path) = &config.whitelist {
        let bytes = std::fs::read(wl_path)?;
        let wl = Whitelist::decode(&bytes)?;
        if !wl.verify(config.admin_pubkey.as_ref()) {
            return Err("BAD whitelist signature".into());
        }
        info!(
            mode = "distributed",
            whitelist = %wl_path.display(),
            admin_pubkey = %hex(&wl.admin_pubkey),
            nodes = wl.len(),
            "loaded signed whitelist"
        );
        let addrs: Vec<String> = wl.entries.iter().map(|e| e.addr.clone()).collect();
        let zs: Vec<u8> = wl.entries.iter().map(|e| e.zone).collect();
        let n = addrs.len();
        (addrs, zs, n)
    } else {
        let base_port: u16 = std::env::var("HOLOFS_EMBED_BASE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9100);
        info!(
            mode = "embedded",
            storage = %config.storage.display(),
            base_port,
            top_port = base_port as usize + N_NODES - 1,
            n_nodes = N_NODES,
            "spawning embedded persistent nodes"
        );
        let mut addrs: Vec<String> = Vec::with_capacity(N_NODES);
        for i in 0..N_NODES {
            let dir = config.storage.join(format!("node_{i:02}"));
            let port = base_port + i as u16;
            let (bound, _store, handle) =
                spawn_node_persistent_with_tls(
                    (Ipv4Addr::LOCALHOST, port).into(),
                    &dir,
                    node_server_cfg.clone(),
                )
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error> {
                        format!(
                            "failed to bind port {port} for node_{i:02}: {e}\n\
                             hint: another holofs binary already running? Stop it or set \
                             HOLOFS_EMBED_BASE_PORT=N to change the base."
                        )
                        .into()
                    })?;
            addrs.push(bound.to_string());
            node_task_handles.push(handle);
        }
        let n_zones: u8 = 4;
        let zone_size = N_NODES / n_zones as usize;
        let zs: Vec<u8> = (0..N_NODES).map(|i| (i / zone_size) as u8).collect();
        info!(
            n_nodes = N_NODES,
            n_zones,
            zone_size,
            "embedded cluster ready"
        );
        (addrs, zs, N_NODES)
    };
    let live: Vec<usize> = (0..n_nodes).collect();

    let catalog_path = config
        .catalog
        .clone()
        .unwrap_or_else(|| config.storage.join("catalog.bin"));
    let mut directory = Directory::load_or_empty(&catalog_path)?;
    // Stage 9 migration: legacy catalogs stored objects under nested keys
    // (`docs/note.txt`) but never wrote explicit `Directory` markers. The
    // new tree-shaped UI requires markers for every prefix, so fill in
    // anything missing and persist before the gateway opens for traffic.
    let synthesized = directory.synthesize_missing_directories();
    if synthesized > 0 {
        info!(
            count = synthesized,
            "synthesized missing directory markers for legacy catalog"
        );
        directory.save_atomic(&catalog_path)?;
    }
    info!(
        path = %catalog_path.display(),
        objects = directory.len(),
        "catalog loaded"
    );

    let should_seed = config.whitelist.is_none() && !config.no_seed && directory.is_empty();
    if should_seed {
        let photo_channels = match config.seed_photo.as_deref() {
            Some(p) => {
                info!(source = %p.display(), "seeding photo.png");
                let a = load_photo(p.to_str().expect("non-utf8 seed path"), w, h);
                vec![a[0].clone(), a[1].clone(), a[2].clone()]
            }
            None if std::path::Path::new("assets/sample.png").exists() => {
                info!(source = "assets/sample.png", "seeding photo.png (Kodak kodim23)");
                let a = load_photo("assets/sample.png", w, h);
                vec![a[0].clone(), a[1].clone(), a[2].clone()]
            }
            None => {
                info!(source = "synthetic mandala", "seeding photo.png");
                let a = synth(w, h);
                vec![a[0].clone(), a[1].clone(), a[2].clone()]
            }
        };
        let m = put_named(&gf, &node_addrs, &zones, &live, &photo_channels, w, h).await;
        directory.insert("photo.png".into(), m);

        let mandala = {
            let a = synth(w, h);
            vec![a[0].clone(), a[1].clone(), a[2].clone()]
        };
        let m = put_named(&gf, &node_addrs, &zones, &live, &mandala, w, h).await;
        directory.insert("mandala.png".into(), m);

        directory.save_atomic(&catalog_path)?;
        info!(objects = directory.len(), "seeded catalog");
    }

    let catalog = Arc::new(Mutex::new(directory));
    let reputation = Arc::new(Mutex::new(Reputation::new(n_nodes, 1.0)));
    let cluster_info = Arc::new(ClusterInfo {
        node_addrs: node_addrs.clone(),
        zones: zones.clone(),
        placement: Placement::RendezvousZoneAware,
        width: w,
        height: h,
    });
    let gateway = Gateway::new_persistent(
        Arc::clone(&gf),
        Arc::clone(&catalog),
        Arc::new(live),
        cluster_info,
        catalog_path.clone(),
    );
    // Stage 12.8: wire the CLIP semantic-search index when the operator
    // opted in. We don't pre-load the model here — that happens lazily
    // on first PUT / first search to keep boot cheap.
    if config.enable_embed {
        let embed_path = config.storage.join("embeddings.bin");
        gateway.enable_embed(embed_path.clone()).await;
        info!(?embed_path, "semantic-search index enabled");
    }
    // Stage 13.4: wire per-object version history when the operator
    // opted in. Side files live at `<storage>/versions/<name>/v…bin`;
    // prior shards stay live on the cluster across PUTs.
    if config.enable_versions {
        let versions_root = config.storage.clone();
        gateway.enable_versions(versions_root.clone()).await;
        // Optional retention cap. `HOLOFS_VERSIONS_KEEP_LAST=N` trims
        // each name's archive to the N most-recent versions on every
        // PUT. Unset / 0 → unlimited history (manual /api/versions/delete
        // remains the only way to free shards).
        let keep_last: usize = std::env::var("HOLOFS_VERSIONS_KEEP_LAST")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if keep_last > 0 {
            gateway.set_versions_keep_last(keep_last).await;
            info!(?versions_root, keep_last, "version history enabled (retention capped)");
        } else {
            info!(?versions_root, "version history enabled");
        }
    }

    let interval_secs: u64 = std::env::var("HOLOFS_MONITOR_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let monitor_cfg = MonitorConfig {
        poll_interval: std::time::Duration::from_secs(interval_secs),
        ..MonitorConfig::default()
    };
    info!(
        poll_secs = monitor_cfg.poll_interval.as_secs(),
        margin_threshold = monitor_cfg.margin_threshold,
        repair_d = monitor_cfg.repair_d,
        rep_threshold = monitor_cfg.reputation_threshold,
        "health monitor configured"
    );
    let mon_catalog = Arc::clone(&catalog);
    let mon_gf = Arc::clone(&gf);
    let mon_rep = Arc::clone(&reputation);
    let mon_shutdown = shutdown.clone();
    let monitor = tokio::spawn(async move {
        run_periodic(
            mon_gf,
            mon_catalog,
            monitor_cfg,
            Some(mon_rep),
            log_event,
            mon_shutdown,
        )
        .await;
    });

    let audit_interval: u64 = std::env::var("HOLOFS_AUDIT_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let audit_cfg = AuditConfig {
        interval: std::time::Duration::from_secs(audit_interval),
        ..AuditConfig::default()
    };
    info!(
        interval_secs = audit_cfg.interval.as_secs(),
        samples = audit_cfg.samples_per_tick,
        threshold = audit_cfg.threshold,
        "PoR auditor configured"
    );
    let aud_catalog = Arc::clone(&catalog);
    let aud_rep = Arc::clone(&reputation);
    let aud_shutdown = shutdown.clone();
    let auditor = tokio::spawn(async move {
        audit::run_periodic(aud_catalog, aud_rep, audit_cfg, log_audit, aud_shutdown).await;
    });

    // Background shard scrub. Walks the catalog every
    // HOLOFS_SCRUB_INTERVAL seconds (default 600 = 10 min) and
    // proactively repairs any object whose `place_shard`-expected
    // hashes are missing from their canonical node. Catches damage
    // from the old reputation cascade, stale node failures, etc.
    // *before* any user GET trips a 503. Set the interval to 0 to
    // disable entirely (the auto-repair-on-read path stays active).
    let scrub_interval_secs: u64 = std::env::var("HOLOFS_SCRUB_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let scrub = if scrub_interval_secs > 0 {
        let scrub_gw = Arc::clone(&gateway);
        let interval = std::time::Duration::from_secs(scrub_interval_secs);
        let scrub_shutdown = shutdown.clone();
        info!(
            interval_secs = scrub_interval_secs,
            "background shard scrub configured"
        );
        Some(tokio::spawn(async move {
            // First tick fires after the interval so we don't hammer
            // the cluster at boot before audit has even started.
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // immediate, but the next is interval-from-now
            loop {
                tokio::select! {
                    _ = scrub_shutdown.cancelled() => return,
                    _ = ticker.tick() => {}
                }
                let report = scrub_gw.scrub_tick().await;
                if report.objects_repaired > 0 || report.objects_repair_failed > 0 {
                    info!(
                        scanned = report.objects_scanned,
                        repaired = report.objects_repaired,
                        failed = report.objects_repair_failed,
                        "scrub tick"
                    );
                }
            }
        }))
    } else {
        info!("background shard scrub disabled (HOLOFS_SCRUB_INTERVAL=0)");
        None
    };

    info!(width = w, height = h, k = K, layers = NLAYERS, "frame parameters");

    // Install the global client TLS config so every gateway RPC honours it.
    // After this point, holofs_client::rpc connects with TLS when the
    // config is Some, plain TCP otherwise.
    holofs_client::transport::set_tls_config(gateway_client_cfg.clone());
    if gateway_client_cfg.is_some() {
        info!(mtls = config.tls.mtls, "TLS active on wire protocol");
    }
    // Keep the signer alive so dynamically-added nodes can be signed later.
    drop(node_signer);

    Ok(Bootstrap {
        gateway,
        monitor,
        auditor,
        scrub,
        node_tasks: node_task_handles,
        shutdown,
    })
}

/// Build an image manifest for the seed step. Copy of the helper in
/// `holofs-http.rs`; both files will be deduplicated in Stage 4d when the
/// legacy gateway binary is retired.
async fn put_named(
    gf: &Gf,
    node_addrs: &[String],
    zones: &[u8],
    live: &[usize],
    channels: &[Vec<f32>],
    w: usize,
    h: usize,
) -> Manifest {
    let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
    for y in 0..h {
        for x in 0..w {
            layer_positions[coeff_layer(x, y, w, h)].push((y * w + x) as u32);
        }
    }
    let n_per_layer: Vec<u32> = (0..NLAYERS)
        .map(|l| (K as f32 * RED[l]).round() as u32)
        .collect();
    let sym_len: Vec<u32> = layer_positions
        .iter()
        .map(|pos| ((pos.len() * 4 + K - 1) / K) as u32)
        .collect();
    let mut m = Manifest {
        object_id: 0,
        k: K as u16,
        nlayers: NLAYERS as u8,
        n_per_layer,
        sym_len,
        layer_positions,
        channels: 3,
        width: w as u32,
        height: h as u32,
        levels: LEVELS as u8,
        nodes: node_addrs.to_vec(),
        placement: Placement::RendezvousZoneAware,
        zones: zones.to_vec(),
        data_cid: [0; 32],
        merkle_root: [0; 32],
        shard_hashes: vec![vec![Vec::new(); NLAYERS]; 3],
        kind: holofs_model::manifest::ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
    };
    put_object(gf, &mut m, &live.to_vec(), channels)
        .await
        .expect("PUT failed during seed");
    m
}

/// Build TLS material for the wire protocol.
///
/// Returns:
/// - `node_server_cfg` — handed to every spawned embedded node (or `None`
///   in distributed mode, where nodes are external processes that own
///   their own server config).
/// - `gateway_client_cfg` — installed via `transport::set_tls_config` so
///   every RPC the gateway makes goes over TLS.
/// - `signer` — kept alive so future code (e.g. dynamically added nodes)
///   can mint additional leaves without re-parsing PEM.
fn build_tls(
    opts: &TlsOptions,
) -> Result<
    (
        Option<Arc<rustls::ServerConfig>>,
        Option<Arc<rustls::ClientConfig>>,
        Option<holofs_storage::tls::CaSigner>,
    ),
    Box<dyn std::error::Error>,
> {
    if !opts.enabled {
        return Ok((None, None, None));
    }
    let (server_mat, client_mat, signer) =
        match (&opts.cert_path, &opts.key_path, &opts.ca_path) {
            (Some(c), Some(k), Some(ca)) => {
                let mat = TlsMaterial::load(c, k, ca, None)?;
                info!(
                    cert = %c.display(),
                    ca = %ca.display(),
                    mtls = opts.mtls,
                    "TLS: loaded operator-supplied PEM material"
                );
                (mat.clone(), mat, None)
            }
            (None, None, None) => {
                let (server_mat, signer) = TlsMaterial::self_signed(
                    "holofs-node",
                    &["127.0.0.1".to_string(), "localhost".to_string()],
                )?;
                let client_mat = if opts.mtls {
                    let gw_leaf =
                        signer.issue_leaf("holofs-gateway", &["127.0.0.1".to_string()])?;
                    TlsMaterial {
                        ca: server_mat.ca.clone(),
                        leaf: gw_leaf,
                    }
                } else {
                    server_mat.clone()
                };
                info!(mtls = opts.mtls, "TLS: generated self-signed CA + leaves (embedded mode)");
                (server_mat, client_mat, Some(signer))
            }
            _ => {
                return Err(
                    "partial TLS config: provide all of --tls-cert, --tls-key, --tls-ca-cert"
                        .into(),
                )
            }
        };
    let server_cfg = server_mat
        .server_config(opts.mtls)
        .map_err(|e| -> Box<dyn std::error::Error> { format!("server config: {e}").into() })?;
    let client_cfg = client_mat
        .client_config(opts.mtls)
        .map_err(|e| -> Box<dyn std::error::Error> { format!("client config: {e}").into() })?;
    Ok((Some(server_cfg), Some(client_cfg), signer))
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

fn log_audit(e: &AuditEvent) {
    match e {
        AuditEvent::Result {
            node,
            object,
            channel,
            layer,
            outcome,
            score_after,
        } => {
            if outcome.is_success() {
                return;
            }
            let tag = match outcome {
                AuditOutcome::Pass => "PASS",
                AuditOutcome::MissingShard => "MISSING",
                AuditOutcome::HashMismatch => "MISMATCH",
                AuditOutcome::Unreachable => "UNREACH",
                AuditOutcome::ProtocolError => "PROTO",
            };
            warn!(
                target: "holofs::audit",
                outcome = tag,
                node = node,
                object = %object,
                channel = channel,
                layer = layer,
                score = %format_args!("{score_after:.2}"),
                "audit failed"
            );
        }
        AuditEvent::LowScore {
            node,
            score,
            threshold,
        } => warn!(
            target: "holofs::audit",
            node = node,
            score = %format_args!("{score:.2}"),
            threshold = %format_args!("{threshold:.2}"),
            "low reputation score"
        ),
    }
}

fn log_event(e: &Event) {
    match e {
        Event::LivenessChange {
            revived,
            died,
            total_live,
            total_nodes,
        } => info!(
            target: "holofs::monitor",
            total_live,
            total_nodes,
            revived = ?revived,
            died = ?died,
            "liveness change"
        ),
        Event::LowMargin {
            object,
            channel,
            layer,
            margin,
        } => warn!(
            target: "holofs::monitor",
            object = %object,
            channel = channel,
            layer = layer,
            margin = margin,
            "low margin"
        ),
        Event::RepairOk {
            object,
            node,
            shards_generated,
            layers_repaired,
        } => info!(
            target: "holofs::monitor",
            object = %object,
            node = node,
            shards_generated,
            layers_repaired,
            "repair ok"
        ),
        Event::RepairFailed {
            object,
            node,
            error,
        } => warn!(
            target: "holofs::monitor",
            object = %object,
            node = node,
            error = %error,
            "repair failed"
        ),
        Event::ScanFailed { object, error } => warn!(
            target: "holofs::monitor",
            object = %object,
            error = %error,
            "scan failed"
        ),
    }
}
