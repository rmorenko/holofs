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

use tokio::sync::{Mutex, RwLock};

use holofs_cluster::audit::{self, AuditConfig, AuditEvent, AuditOutcome};
use holofs_cluster::monitor::{run_periodic, Event, MonitorConfig};
use holofs_cluster::reputation::Reputation;
use holofs_codec::image_io::synth;
use holofs_core::gf::Gf;
use holofs_core::hash::hex;
use holofs_core::{dims_from_env, K, NLAYERS, N_NODES};
use holofs_gateway::catalog_store::{migrate_legacy_if_present, CatalogStore};
use holofs_gateway::util::encode_png;
use holofs_gateway::{ClusterInfo, Gateway};
use holofs_model::fs::Directory;
use holofs_model::placement::Placement;
use holofs_storage::identity::PUBKEY_LEN;
use holofs_storage::node_service::spawn_node_persistent_with_tls;
use holofs_storage::tls::TlsMaterial;
use holofs_storage::whitelist::Whitelist;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::supervised::supervised_spawn;

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
    /// enable CLIP-based semantic search. Index lives at
    /// `<storage>/embeddings.bin`; first inference downloads ~155 MiB
    /// of model weights into `~/.cache/huggingface/hub`.
    pub enable_embed: bool,
    /// enable per-object version history. Side files under
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
    /// Build a config from the process-wide [`RuntimeConfig`] snapshot
    /// (which itself is populated from env at bootstrap). Prior to
    /// S4-5 this method inlined every `HOLOFS_*` env read — a typo
    /// here vs. the twin read in `main.rs` was a real regression
    /// vector. Now every knob has one canonical source.
    pub fn from_env() -> Self {
        let cfg = crate::runtime_config::RuntimeConfig::init();
        let admin_pubkey = cfg
            .storage
            .admin_pubkey
            .as_deref()
            .and_then(parse_pubkey_hex);
        Self {
            storage: cfg.storage.storage_dir.clone(),
            catalog: cfg.storage.catalog.clone(),
            whitelist: cfg.storage.whitelist.clone(),
            admin_pubkey,
            seed_photo: cfg.storage.seed_photo.clone(),
            no_seed: cfg.storage.no_seed,
            tls: TlsOptions::default(),
            enable_embed: cfg.features.enable_embed,
            enable_versions: cfg.features.enable_versions,
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
    /// N5: throttled reputation-state persistence task. Snapshots the
    /// shared `Arc<Mutex<Reputation>>` every
    /// `HOLOFS_REPUTATION_PERSIST_INTERVAL` seconds (default 30) and
    /// on shutdown so a restart resumes with the last-known scores.
    pub reputation_persist: tokio::task::JoinHandle<()>,
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

        // P0.3b client-side: load our own gateway identity + install
        // the pool-wide auth material so every outbound wire dial
        // completes the bilateral handshake against the whitelisted
        // node pubkey. Nodes running in `--client-whitelist` strict
        // mode need the pubkey we log here on their whitelist;
        // nodes still in permissive mode accept the handshake but
        // don't verify, so mixed rollouts work.
        //
        // Identity path: `HOLOFS_GATEWAY_IDENTITY_KEY` env override,
        // else `<storage>/gateway_identity.key`. `load_or_create`
        // seeds a fresh keypair on first boot and persists it — the
        // pubkey is stable across restarts, safe to bake into a
        // signed node whitelist.
        let identity_path = std::env::var_os("HOLOFS_GATEWAY_IDENTITY_KEY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config.storage.join("gateway_identity.key"));
        let gw_identity = holofs_storage::identity::NodeIdentity::load_or_create(&identity_path)
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("gateway identity {}: {e}", identity_path.display()).into()
            })?;
        info!(
            gateway_identity = %identity_path.display(),
            gateway_pubkey = %hex(&gw_identity.pubkey()),
            "loaded gateway identity (add this pubkey to each node's --client-whitelist)"
        );
        let mut node_pubkeys: std::collections::HashMap<String, [u8; PUBKEY_LEN]> =
            std::collections::HashMap::with_capacity(wl.entries.len());
        for e in &wl.entries {
            node_pubkeys.insert(e.addr.clone(), e.pubkey);
        }
        holofs_client::pool::set_client_auth(Some(std::sync::Arc::new(
            holofs_client::pool::ClientAuthConfig {
                identity: gw_identity,
                node_pubkeys,
            },
        )));

        let addrs: Vec<String> = wl.entries.iter().map(|e| e.addr.clone()).collect();
        let zs: Vec<u8> = wl.entries.iter().map(|e| e.zone).collect();
        let n = addrs.len();
        (addrs, zs, n)
    } else {
        let base_port: u16 = crate::runtime_config::RuntimeConfig::get()
            .embed_cluster
            .base_port;
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
            let (bound, _store, handle) = spawn_node_persistent_with_tls(
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
            n_zones, zone_size, "embedded cluster ready"
        );
        (addrs, zs, N_NODES)
    };
    let live: Vec<usize> = (0..n_nodes).collect();

    let catalog_path = config
        .catalog
        .clone()
        .unwrap_or_else(|| config.storage.join("catalog.bin"));

    // Post-P0.1c: catalog lives in redb, not in a whole-file
    // `catalog.bin`. `catalog_path` above stays as a "where does the
    // catalog live?" label — the actual bytes live in
    // `<dir>/catalog.redb`.
    //
    // Boot sequence:
    //   1. `migrate_legacy_if_present` — if this install still has
    //      the old whole-file catalog next to us and no redb yet,
    //      one-shot replay of the legacy entries into a fresh redb
    //      and rename the legacy file to `.migrated-<epoch>` as a
    //      safety backup. Idempotent.
    //   2. Open the redb, replay every persisted entry into a fresh
    //      in-memory `Directory`.
    //   3. Async-ingest recovery + missing-directory synthesis run
    //      against that Directory; each mutation writes just the
    //      touched entries back to redb (no whole-catalog rewrites
    //      anywhere in the boot path).
    let catalog_dir = catalog_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| config.storage.clone());
    // Also honour the legacy filename callers may have configured
    // via `--catalog <path>/catalog.bin`. If a `catalog.bin` sits
    // next to the redb, treat it the same as `catalog.holo`.
    let legacy_bin = catalog_dir.join("catalog.bin");
    if legacy_bin.exists() && !catalog_dir.join("catalog.holo").exists() {
        let holo = catalog_dir.join("catalog.holo");
        std::fs::rename(&legacy_bin, &holo).ok();
    }
    if let Some(backup) = migrate_legacy_if_present(&catalog_dir)
        .map_err(|e| anyhow::anyhow!("catalog migration failed: {e}"))?
    {
        info!(
            backup = %backup.display(),
            "catalog: migrated legacy catalog.holo → catalog.redb (legacy file backed up)"
        );
    }
    let catalog_store = std::sync::Arc::new(
        CatalogStore::open(&catalog_dir).map_err(|e| anyhow::anyhow!("catalog store open: {e}"))?,
    );
    let mut directory = Directory::new();
    let loaded = catalog_store
        .load_into(&mut directory)
        .map_err(|e| anyhow::anyhow!("catalog store load: {e}"))?;

    // Async-ingest recovery: any manifest still marked `Encoding` at
    // boot time is from an in-flight PUT that lost its worker to a
    // process restart. Downgrade to `Failed` so reads treat it as
    // absent and a subsequent PUT can replace it. Persist so the
    // next unclean shutdown doesn't chain-corrupt the same entries.
    {
        use holofs_model::manifest::ManifestState;
        let mut promoted_names: Vec<(String, Vec<u8>)> = Vec::new();
        for (name, arc) in directory.entries.iter_mut() {
            if arc.state == ManifestState::Encoding {
                std::sync::Arc::make_mut(arc).state = ManifestState::Failed;
                promoted_names.push((name.clone(), arc.encode()));
            }
        }
        if !promoted_names.is_empty() {
            let count = promoted_names.len();
            info!(
                count,
                "async-ingest recovery: marked orphaned Encoding manifests as Failed"
            );
            catalog_store
                .apply_batch(promoted_names, std::iter::empty())
                .map_err(|e| anyhow::anyhow!("catalog boot-recovery persist: {e}"))?;
        }
    }
    // migration: legacy catalogs stored objects under nested keys
    // (`docs/note.txt`) but never wrote explicit `Directory` markers. The
    // new tree-shaped UI requires markers for every prefix, so fill in
    // anything missing and persist before the gateway opens for traffic.
    // synthesize_missing_directories mutates in place; we re-persist
    // by walking the whole tree once (one-shot cost, only fires on
    // legacy catalogs).
    let synthesized = directory.synthesize_missing_directories();
    if synthesized > 0 {
        info!(
            count = synthesized,
            "synthesized missing directory markers for legacy catalog"
        );
        let batch: Vec<(String, Vec<u8>)> = directory
            .entries
            .iter()
            .map(|(n, m)| (n.clone(), m.encode()))
            .collect();
        catalog_store
            .apply_batch(batch, std::iter::empty())
            .map_err(|e| anyhow::anyhow!("catalog synthesize persist: {e}"))?;
    }
    info!(
        path = %catalog_store.path().display(),
        objects = loaded,
        "catalog loaded"
    );

    let should_seed = config.whitelist.is_none() && !config.no_seed && directory.is_empty();
    let catalog = Arc::new(RwLock::new(directory));

    // N5: reputation state persists across restarts. Path fixed at
    // `<storage>/reputation.bin`. Any load failure (missing, corrupt,
    // n_nodes mismatch) silently falls back to a fresh table so a
    // P1.5 audit log — install the process-wide singleton before
    // axum starts serving so the first admin request already flows
    // through it. HOLOFS_AUDIT_LOG env override handled inside
    // `AuditLogger::open`; unset ⇒ `<storage>/audit.log`, `off` ⇒
    // no-op logger. IO failure downgrades to disabled with an
    // eprintln, never blocks boot.
    let audit_logger = crate::audit::AuditLogger::open(&config.storage);
    match audit_logger.path() {
        Some(p) => info!(path = %p.display(), "audit log enabled"),
        None => info!("audit log disabled (HOLOFS_AUDIT_LOG=off or open failed)"),
    }
    crate::audit::set_global(std::sync::Arc::clone(&audit_logger));

    // rewired cluster boots successfully.
    let reputation_path = config.storage.join("reputation.bin");
    let (reputation_state, rep_loaded) = Reputation::load_or_new(&reputation_path, n_nodes, 1.0);
    if rep_loaded {
        info!(
            path = %reputation_path.display(),
            n_nodes,
            "reputation state loaded from disk"
        );
    } else {
        info!(
            path = %reputation_path.display(),
            n_nodes,
            "reputation state seeded fresh"
        );
    }
    let reputation = Arc::new(Mutex::new(reputation_state));
    let cluster_info = Arc::new(ClusterInfo {
        node_addrs: node_addrs.clone(),
        zones: zones.clone(),
        placement: Placement::RendezvousZoneAware,
        width: w,
        height: h,
    });
    let mut gateway = Gateway::new_with_catalog_store(
        Arc::clone(&gf),
        Arc::clone(&catalog),
        Arc::new(live),
        cluster_info,
        catalog_path.clone(),
        Arc::clone(&catalog_store),
    );
    // N3: apply env-configured backpressure caps before anything else
    // gets an Arc handle. `Arc::get_mut` succeeds only while the
    // refcount is 1 — right here, before any spawn or clone — so we
    // don't need interior mutability on the semaphore fields.
    // Every knob comes from the typed `RuntimeConfig` snapshot (S4-5)
    // so the resolve-once, read-many pattern replaces the four inline
    // env::var lookups this block used to run.
    let rc = crate::runtime_config::RuntimeConfig::get();
    let medium_cap = rc.reliability.medium_concurrency;
    let long_cap = rc.reliability.long_concurrency;
    let encode_cap = rc.reliability.encode_concurrency;
    let encode_queue_max = rc.reliability.encode_queue_max;
    if let Some(gw_mut) = Arc::get_mut(&mut gateway) {
        gw_mut.configure_limits(medium_cap, long_cap);
        gw_mut.configure_encode_limit(encode_cap);
        gw_mut.configure_encode_queue_max(encode_queue_max);
    } else {
        warn!("Arc<Gateway> refcount already > 1 after construction; backpressure caps stayed at defaults");
    }
    info!(
        medium_cap,
        long_cap, encode_cap, encode_queue_max, "N3 backpressure caps applied"
    );
    // wire the CLIP semantic-search index when the operator
    // opted in. We don't pre-load the model here — that happens lazily
    // on first PUT / first search to keep boot cheap.
    if config.enable_embed {
        let embed_path = config.storage.join("embeddings.bin");
        gateway.enable_embed(embed_path.clone()).await;
        info!(?embed_path, "semantic-search index enabled");
    }
    // wire per-object version history when the operator
    // opted in. Side files live at `<storage>/versions/<name>/v…bin`;
    // prior shards stay live on the cluster across PUTs.
    if config.enable_versions {
        let versions_root = config.storage.clone();
        gateway.enable_versions(versions_root.clone()).await;
        // Optional retention cap. `HOLOFS_VERSIONS_KEEP_LAST=N` trims
        // each name's archive to the N most-recent versions on every
        // PUT. Unset / 0 → unlimited history (manual /api/versions/delete
        // remains the only way to free shards).
        let keep_last: usize = crate::runtime_config::RuntimeConfig::get()
            .reliability
            .versions_keep_last
            .unwrap_or(0);
        if keep_last > 0 {
            gateway.set_versions_keep_last(keep_last).await;
            info!(
                ?versions_root,
                keep_last, "version history enabled (retention capped)"
            );
        } else {
            info!(?versions_root, "version history enabled");
        }
    }

    // v3-11: seed the catalog through Gateway::ingest_bytes so demo objects
    // travel the exact ingest path a `PUT /photo.png` request would take —
    // dedup, auto-repair-on-read, versions, embed indexing. The previous
    // `put_named` helper reimplemented Manifest construction + `put_object`
    // by hand, which drifted from `ingest_bytes` every time the encoder
    // pipeline changed. Runs after `enable_embed` / `enable_versions` so
    // opted-in features apply to the seed objects too.
    if should_seed {
        let (photo_bytes, photo_src): (Vec<u8>, &str) = match config.seed_photo.as_deref() {
            Some(p) => (std::fs::read(p)?, "operator-supplied file"),
            None if std::path::Path::new("assets/sample.png").exists() => (
                std::fs::read("assets/sample.png")?,
                "assets/sample.png (Kodak kodim23)",
            ),
            None => {
                let a = synth(w, h);
                (
                    encode_png(
                        &[a[0].clone(), a[1].clone(), a[2].clone()],
                        w as u32,
                        h as u32,
                    ),
                    "synthetic mandala",
                )
            }
        };
        info!(source = photo_src, "seeding photo.png");
        gateway
            .ingest_bytes("photo.png", &photo_bytes)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("seed photo.png failed: {e:?}").into()
            })?;

        let mandala_bytes = {
            let a = synth(w, h);
            encode_png(
                &[a[0].clone(), a[1].clone(), a[2].clone()],
                w as u32,
                h as u32,
            )
        };
        info!(source = "synthetic mandala", "seeding mandala.png");
        gateway
            .ingest_bytes("mandala.png", &mandala_bytes)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("seed mandala.png failed: {e:?}").into()
            })?;

        let objects = catalog.read().await.len();
        info!(objects, "seeded catalog");
    }

    let interval_secs: u64 = crate::runtime_config::RuntimeConfig::get()
        .reliability
        .monitor_interval_secs;
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
    // N8: task-restart counters exposed in /metrics. Destructure
    // now so each supervised_spawn call gets its own Arc handle.
    // `restarts_scrub` may go unused when the operator disabled the
    // scrub loop via `HOLOFS_SCRUB_INTERVAL=0` — the `_` prefix
    // suppresses the warning without hiding it from grep.
    #[allow(unused_variables)]
    let (restarts_monitor, restarts_auditor, restarts_scrub) = gateway.task_restart_counters();
    let mon_catalog = Arc::clone(&catalog);
    let mon_gf = Arc::clone(&gf);
    let mon_rep = Arc::clone(&reputation);
    let mon_shutdown = shutdown.clone();
    let monitor = supervised_spawn("monitor", shutdown.clone(), restarts_monitor, move || {
        let gf = Arc::clone(&mon_gf);
        let cat = Arc::clone(&mon_catalog);
        let rep = Arc::clone(&mon_rep);
        let sd = mon_shutdown.clone();
        let cfg = monitor_cfg.clone();
        async move {
            run_periodic(gf, cat, cfg, Some(rep), log_event, sd).await;
        }
    });

    let audit_interval: u64 = crate::runtime_config::RuntimeConfig::get()
        .reliability
        .audit_interval_secs;
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
    let auditor = supervised_spawn("auditor", shutdown.clone(), restarts_auditor, move || {
        let cat = Arc::clone(&aud_catalog);
        let rep = Arc::clone(&aud_rep);
        let sd = aud_shutdown.clone();
        let cfg = audit_cfg.clone();
        async move {
            audit::run_periodic(cat, rep, cfg, log_audit, sd).await;
        }
    });

    // N5: throttled reputation persistence. Snapshot every
    // `HOLOFS_REPUTATION_PERSIST_INTERVAL` seconds (default 30s ==
    // one auditor cycle by default) and atomic-rename over
    // `<storage>/reputation.bin`. We also snapshot once at
    // shutdown so the last observations don't get lost.
    let reputation_persist_secs: u64 = crate::runtime_config::RuntimeConfig::get()
        .reliability
        .reputation_persist_interval_secs;
    let rep_persist_path = reputation_path.clone();
    let rep_persist_rep = Arc::clone(&reputation);
    let rep_persist_shutdown = shutdown.clone();
    // Reuse the auditor's restart counter for now — reputation
    // persistence is close-kin to audit and we don't want to grow
    // ObservabilityCounters for a task the operator can't do
    // anything about individually.
    let rep_persist_task = supervised_spawn(
        "reputation-persist",
        shutdown.clone(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        move || {
            let path = rep_persist_path.clone();
            let rep = Arc::clone(&rep_persist_rep);
            let sd = rep_persist_shutdown.clone();
            let interval = std::time::Duration::from_secs(reputation_persist_secs);
            async move {
                let mut ticker = tokio::time::interval(interval);
                // Skip the immediate first tick — we already know
                // the state is fresh (or freshly-loaded) at boot.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = sd.cancelled() => {
                            // Final save on shutdown so the last
                            // batch of audit observations survives.
                            let snap = rep.lock().await.clone();
                            if let Err(e) = snap.save_atomic(&path) {
                                tracing::error!(
                                    error = %e,
                                    path = %path.display(),
                                    "reputation final-save failed"
                                );
                            } else {
                                tracing::info!("reputation final-save ok");
                            }
                            return;
                        }
                        _ = ticker.tick() => {}
                    }
                    let snap = rep.lock().await.clone();
                    if let Err(e) = snap.save_atomic(&path) {
                        tracing::error!(
                            error = %e,
                            path = %path.display(),
                            "reputation persist failed"
                        );
                    }
                }
            }
        },
    );

    // Background shard scrub. Walks the catalog every
    // HOLOFS_SCRUB_INTERVAL seconds (default 600 = 10 min) and
    // proactively repairs any object whose `place_shard`-expected
    // hashes are missing from their canonical node. Catches damage
    // from the old reputation cascade, stale node failures, etc.
    // *before* any user GET trips a 503. Set the interval to 0 to
    // disable entirely (the auto-repair-on-read path stays active).
    let scrub_interval_secs: u64 = crate::runtime_config::RuntimeConfig::get()
        .reliability
        .scrub_interval_secs;
    let scrub = if scrub_interval_secs > 0 {
        let scrub_gw = Arc::clone(&gateway);
        let interval = std::time::Duration::from_secs(scrub_interval_secs);
        let scrub_shutdown = shutdown.clone();
        info!(
            interval_secs = scrub_interval_secs,
            "background shard scrub configured"
        );
        Some(supervised_spawn(
            "scrub",
            shutdown.clone(),
            restarts_scrub,
            move || {
                let gw = Arc::clone(&scrub_gw);
                let sd = scrub_shutdown.clone();
                async move {
                    // First tick fires after the interval so we don't hammer
                    // the cluster at boot before audit has even started.
                    let mut ticker = tokio::time::interval(interval);
                    ticker.tick().await; // immediate, but the next is interval-from-now
                    loop {
                        tokio::select! {
                            _ = sd.cancelled() => return,
                            _ = ticker.tick() => {}
                        }
                        let report = gw.scrub_tick().await;
                        if report.objects_repaired > 0 || report.objects_repair_failed > 0 {
                            info!(
                                scanned = report.objects_scanned,
                                repaired = report.objects_repaired,
                                failed = report.objects_repair_failed,
                                "scrub tick"
                            );
                        }
                    }
                }
            },
        ))
    } else {
        info!("background shard scrub disabled (HOLOFS_SCRUB_INTERVAL=0)");
        None
    };

    info!(
        width = w,
        height = h,
        k = K,
        layers = NLAYERS,
        "frame parameters"
    );

    // P1.4b — spawn the capacity poller (refreshes gateway.capacity_map
    // every ~60 s) and, when enabled, the auto-rebalancer daemon
    // (drains the fullest node when used_pct crosses a threshold).
    // Both live for the process lifetime; drop-on-shutdown via the
    // JoinHandle they return is enough (no cancellation token needed —
    // they only stall on interval ticks).
    let poll_interval_secs: u64 = std::env::var("HOLOFS_CAPACITY_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(holofs_gateway::capacity::DEFAULT_POLL_INTERVAL_SECS);
    let _poller = holofs_gateway::capacity::spawn_capacity_poller(
        Arc::clone(gateway.cluster()),
        Arc::clone(&gateway.capacity_map),
        poll_interval_secs,
    );
    info!(poll_interval_secs, "capacity poller started");

    let rebalance_interval_secs: u64 = std::env::var("HOLOFS_REBALANCE_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(holofs_gateway::capacity::DEFAULT_REBALANCE_INTERVAL_SECS);
    let rebalance_trigger_pct: f64 = std::env::var("HOLOFS_REBALANCE_TRIGGER_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(holofs_gateway::capacity::DEFAULT_REBALANCE_TRIGGER_PCT);
    let rebalance_cold_ceiling_pct: f64 = std::env::var("HOLOFS_REBALANCE_COLD_CEILING_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(holofs_gateway::capacity::DEFAULT_REBALANCE_COLD_CEILING_PCT);
    let repair_d_for_rebalance = K; // same donor count the manual drain uses
    if let Some(_h) = holofs_gateway::capacity::spawn_auto_rebalancer(
        Arc::clone(&gateway),
        Arc::clone(&gateway.capacity_map),
        Arc::clone(&catalog),
        rebalance_interval_secs,
        rebalance_trigger_pct,
        rebalance_cold_ceiling_pct,
        repair_d_for_rebalance,
    ) {
        info!(
            rebalance_interval_secs,
            rebalance_trigger_pct, rebalance_cold_ceiling_pct, "auto-rebalance daemon started"
        );
    } else {
        info!(
            "auto-rebalance daemon disabled \
             (HOLOFS_REBALANCE_INTERVAL_SECS=0 or HOLOFS_REBALANCE_TRIGGER_PCT<=0)"
        );
    }

    // P2.2 — retention GC daemon. Ticks every N seconds, walks the
    // catalog, deletes every non-directory object whose retention
    // policy tripped by wall-clock. Env
    // `HOLOFS_RETENTION_GC_INTERVAL_SECS=0` disables entirely.
    let retention_interval_secs: u64 = std::env::var("HOLOFS_RETENTION_GC_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(holofs_gateway::retention::DEFAULT_RETENTION_GC_INTERVAL_SECS);
    if let Some(_h) = holofs_gateway::retention::spawn_retention_gc(
        Arc::clone(&gateway),
        Arc::clone(&catalog),
        retention_interval_secs,
    ) {
        info!(retention_interval_secs, "retention GC daemon started");
    } else {
        info!("retention GC daemon disabled (HOLOFS_RETENTION_GC_INTERVAL_SECS=0)");
    }

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
        reputation_persist: rep_persist_task,
        node_tasks: node_task_handles,
        shutdown,
    })
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
    let (server_mat, client_mat, signer) = match (&opts.cert_path, &opts.key_path, &opts.ca_path) {
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
                let gw_leaf = signer.issue_leaf("holofs-gateway", &["127.0.0.1".to_string()])?;
                TlsMaterial {
                    ca: server_mat.ca.clone(),
                    leaf: gw_leaf,
                }
            } else {
                server_mat.clone()
            };
            info!(
                mtls = opts.mtls,
                "TLS: generated self-signed CA + leaves (embedded mode)"
            );
            (server_mat, client_mat, Some(signer))
        }
        _ => {
            return Err(
                "partial TLS config: provide all of --tls-cert, --tls-key, --tls-ca-cert".into(),
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
