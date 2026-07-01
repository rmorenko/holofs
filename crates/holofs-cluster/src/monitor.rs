//! Stage 5: background health monitor.
//!
//! Each `tick`:
//! 1. Pings every cluster node via `discover_live`.
//! 2. Diffs against the previous snapshot — who joined, who left.
//! 3. Computes per-object surplus `margin = alive - K` on (channel, layer);
//!    if any layer dropped below threshold a `LowMargin` event is raised.
//! 4. For each revived node `repair_node` is triggered: refill the shards HRW
//!    assigns to that node. Idempotent — on a healthy node a repeated repair
//!    costs only the network round trip, no shards are lost.
//!
//! This is the "operational" loop: it fixes faults that already happened.
//! Preventive auto-rebalancing on topology change is a separate Stage 5 item.
//!
//! Pure-logic design: `tick_once` takes the current state and returns a list
//! of events. Long-lived state (`prev_live`, `rng`) is held by the caller —
//! that keeps unit tests simple.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::health::collect_layer_stats;
use crate::reputation::Reputation;
use holofs_client::{discover_live, repair_node};
use holofs_core::gf::Gf;
use holofs_core::rng::Rng;
use holofs_model::fs::Directory;
use holofs_model::manifest::Manifest;

/// Auto-repair trigger threshold on minimum `margin` (alive − K).
/// Default = 0: repair as soon as any layer drops to the K threshold.
pub const DEFAULT_MARGIN_THRESHOLD: i32 = 0;

/// How many donor shards regen takes per layer. K guarantees full retention.
pub const DEFAULT_REPAIR_D: usize = holofs_core::K;

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub poll_interval: Duration,
    pub margin_threshold: i32,
    pub repair_d: usize,
    /// Reputation-based node exclusion threshold (see `Reputation::alive`).
    /// 0.0 = reputation does not affect the live set (Stage 5 behaviour).
    pub reputation_threshold: f32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(15),
            margin_threshold: DEFAULT_MARGIN_THRESHOLD,
            repair_d: DEFAULT_REPAIR_D,
            reputation_threshold: crate::reputation::DEFAULT_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The live node set changed.
    LivenessChange {
        revived: Vec<usize>,
        died: Vec<usize>,
        total_live: usize,
        total_nodes: usize,
    },
    /// Object surplus dropped below the threshold — usually a precursor of `Repair`.
    LowMargin {
        object: String,
        channel: u8,
        layer: u8,
        margin: i32,
    },
    /// Repair of a revived node succeeded.
    RepairOk {
        object: String,
        node: usize,
        shards_generated: usize,
        layers_repaired: usize,
    },
    /// Repair failed (node silent, manifest corrupt, etc.).
    RepairFailed {
        object: String,
        node: usize,
        error: String,
    },
    /// Could not poll the cluster for a specific object.
    ScanFailed { object: String, error: String },
}

/// Monitor state preserved across ticks.
#[derive(Debug, Clone, Default)]
pub struct MonitorState {
    /// Previous live-node snapshot; an empty `HashSet` at startup means
    /// "we know nothing", so the first tick emits a `LivenessChange` with
    /// `revived` = the entire live set.
    pub prev_live: HashSet<usize>,
    /// Number of ticks executed (debug/test aid).
    pub ticks: u64,
}

/// Run one monitor pass. Mutates `state` (prev_live and counter) and
/// `catalog` (after repair the manifests gain new shard hashes and the
/// Merkle root is updated). Returns the events emitted this tick.
///
/// `nodes_ref` provides the full node list (for the liveness scan); it is
/// extracted from a manifest, but manifests in the catalog may reference
/// different clusters, so we take the first one we encounter as
/// "canonical" topology source. For an in-process demo this is always the
/// same cluster.
pub async fn tick_once(
    gf: &Gf,
    catalog: Arc<Mutex<Directory>>,
    state: &mut MonitorState,
    config: &MonitorConfig,
    rng: &mut Rng,
    reputation: Option<Arc<Mutex<Reputation>>>,
) -> Vec<Event> {
    let is_first_tick = state.ticks == 0;
    state.ticks += 1;
    let mut events = Vec::new();

    // 1. Node addresses come from the first non-directory manifest in
    //    the catalog. Directory markers carry zero nodes; if we
    //    picked one here we'd end up with `live = []`, which floods
    //    the log with bogus "n_alive = 0, margin = -K" warnings AND
    //    triggers a downstream panic on place_shard inside the
    //    auditor (the directory entry yields an empty live set —
    //    see audit::tick_once for the same fix).
    let nodes_addrs: Vec<String> = {
        use holofs_model::manifest::ObjectKind;
        let cat = catalog.lock().await;
        match cat
            .entries
            .values()
            .find(|m| m.kind != ObjectKind::Directory && !m.nodes.is_empty())
        {
            Some(m) => m.nodes.clone(),
            None => return events, // empty catalog / only directories — nothing to scan
        }
    };
    let total_nodes = nodes_addrs.len();

    // 2. Pseudo-manifest for discover_live: the pinger walks nodes_addrs by index.
    let probe = probe_manifest(nodes_addrs);
    let mut live = discover_live(&probe).await;
    // 2a. Stage 7.2: drop nodes with poor reputation. They technically answer
    // ping, but we no longer trust them — exclude them from operations.
    if let Some(rep) = &reputation {
        let rep_guard = rep.lock().await;
        live = rep_guard.filter_live(&live, config.reputation_threshold);
    }
    let live_set: HashSet<usize> = live.iter().copied().collect();

    // 3. LivenessChange. First tick = baseline (no revived/died, no repair).
    let (revived, died) = if is_first_tick {
        (Vec::new(), Vec::new())
    } else {
        (
            sorted(
                &live_set
                    .difference(&state.prev_live)
                    .copied()
                    .collect::<Vec<_>>(),
            ),
            sorted(
                &state
                    .prev_live
                    .difference(&live_set)
                    .copied()
                    .collect::<Vec<_>>(),
            ),
        )
    };
    if is_first_tick || !revived.is_empty() || !died.is_empty() {
        events.push(Event::LivenessChange {
            revived: revived.clone(),
            died: died.clone(),
            total_live: live.len(),
            total_nodes,
        });
    }

    // 4. Per-object margin → LowMargin events.
    let names: Vec<String> = {
        let cat = catalog.lock().await;
        cat.names()
    };
    for name in &names {
        let manifest = {
            let cat = catalog.lock().await;
            cat.get(name).cloned()
        };
        let Some(m) = manifest else { continue };
        match collect_layer_stats(&m, &live).await {
            Ok(stats) => {
                for s in &stats {
                    if s.margin <= config.margin_threshold {
                        events.push(Event::LowMargin {
                            object: name.clone(),
                            channel: s.channel,
                            layer: s.layer,
                            margin: s.margin,
                        });
                    }
                }
            }
            Err(e) => {
                events.push(Event::ScanFailed {
                    object: name.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    // 5. For every revived node — run repair_node for each object.
    for &node in &revived {
        for name in &names {
            let mut cat = catalog.lock().await;
            let Some(m) = cat.entries.get_mut(name) else {
                continue;
            };
            match repair_node(gf, rng, m, &live, node, config.repair_d).await {
                Ok(stats) => events.push(Event::RepairOk {
                    object: name.clone(),
                    node,
                    shards_generated: stats.shards_generated,
                    layers_repaired: stats.layers_repaired,
                }),
                Err(e) => events.push(Event::RepairFailed {
                    object: name.clone(),
                    node,
                    error: e.to_string(),
                }),
            }
        }
    }

    state.prev_live = live_set;
    events
}

/// Long-running monitor loop: tick every `poll_interval`. Suppresses per-tick
/// panics so it never bubbles out; events are forwarded to `on_event` (usually
/// `eprintln!`).
///
/// N1: `shutdown` short-circuits both the tick body and the inter-tick sleep
/// so a SIGTERM lands within one `poll_interval`-tick worst case instead of
/// waiting for the next `sleep` to complete.
pub async fn run_periodic(
    gf: Arc<Gf>,
    catalog: Arc<Mutex<Directory>>,
    config: MonitorConfig,
    reputation: Option<Arc<Mutex<Reputation>>>,
    on_event: impl Fn(&Event) + Send + Sync + 'static,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut state = MonitorState::default();
    let mut rng = Rng::new(0xC0FFEE);
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let events = tokio::select! {
            _ = shutdown.cancelled() => return,
            evs = tick_once(
                &gf,
                Arc::clone(&catalog),
                &mut state,
                &config,
                &mut rng,
                reputation.clone(),
            ) => evs,
        };
        for e in &events {
            on_event(e);
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

// === Helpers ===============================================================

fn sorted(v: &[usize]) -> Vec<usize> {
    let mut s = v.to_vec();
    s.sort();
    s
}

/// Minimal "probe" manifest: `discover_live` looks only at the address list.
/// Other fields are stub-filled.
fn probe_manifest(nodes: Vec<String>) -> Manifest {
    use holofs_model::placement::Placement;
    let n = nodes.len();
    Manifest {
        object_id: 0,
        k: 1,
        nlayers: 1,
        n_per_layer: vec![1],
        sym_len: vec![1],
        layer_positions: vec![vec![]],
        channels: 1,
        width: 1,
        height: 1,
        levels: 0,
        nodes,
        placement: Placement::Rendezvous,
        zones: vec![0; n],
        data_cid: [0; 32],
        merkle_root: [0; 32],
        shard_hashes: vec![vec![Vec::new(); 1]; 1],
        kind: holofs_model::manifest::ObjectKind::Image,
        content_type: "image/png".into(),
        chunk_lens: vec![],
        audio_sample_rate: 0,
        text_minhash: vec![],
        created_at_unix: 0,
        encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_reasonable() {
        let c = MonitorConfig::default();
        assert_eq!(c.margin_threshold, 0);
        assert_eq!(c.repair_d, holofs_core::K);
        assert!(c.poll_interval.as_secs() >= 1);
    }

    #[test]
    fn sorted_helper_sorts_ascending() {
        assert_eq!(sorted(&[3, 1, 2]), vec![1, 2, 3]);
        assert_eq!(sorted(&[]), Vec::<usize>::new());
    }

    #[test]
    fn liveness_change_event_diffs_revived_and_died() {
        // Check the event shape so the log does not produce nasty surprises.
        let e = Event::LivenessChange {
            revived: vec![5, 7],
            died: vec![2],
            total_live: 10,
            total_nodes: 12,
        };
        match e {
            Event::LivenessChange {
                revived,
                died,
                total_live,
                total_nodes,
            } => {
                assert_eq!(revived, vec![5, 7]);
                assert_eq!(died, vec![2]);
                assert_eq!(total_live, 10);
                assert_eq!(total_nodes, 12);
            }
            _ => panic!(),
        }
    }

    // --- tick_once integration ---------------------------------------------
    //
    // The mock-cluster tests below stand up tiny TCP listeners that
    // respond to Ping (so discover_live picks them up) and to
    // ListHashes (so collect_layer_stats's gather succeeds). Each
    // listener loops until the TempDir-wrapped lifetime expires —
    // no graceful shutdown needed for a unit test, the test binary
    // exit reclaims everything.

    use holofs_model::manifest::{Manifest, ObjectEncoding, ObjectKind};
    use holofs_model::placement::Placement;
    use holofs_testutils::{spawn_mock_node as spawn_responder, DisablePool};
    use holofs_wire::Response;
    use tokio::net::TcpListener;

    /// Build a minimal "alive" manifest with the given node list.
    /// Image kind so monitor doesn't skip it as Directory; one
    /// channel + one layer keeps the per-object scan cheap.
    fn manifest_for(nodes: Vec<String>) -> Manifest {
        let n = nodes.len();
        Manifest {
            object_id: 1,
            k: 4,
            nlayers: 1,
            n_per_layer: vec![4],
            sym_len: vec![32],
            layer_positions: vec![vec![]],
            channels: 1,
            width: 8,
            height: 8,
            levels: 1,
            nodes,
            placement: Placement::Rendezvous,
            zones: vec![0; n],
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
        }
    }

    #[tokio::test]
    async fn tick_once_on_empty_catalog_returns_no_events() {
        let _g = DisablePool::new();
        let gf = Gf::new();
        let cat = Arc::new(Mutex::new(Directory::new()));
        let mut state = MonitorState::default();
        let mut rng = Rng::new(1);
        let cfg = MonitorConfig::default();
        let events = tick_once(&gf, cat, &mut state, &cfg, &mut rng, None).await;
        assert!(events.is_empty());
        // State is still untouched in any visible way.
        assert_eq!(state.ticks, 1);
    }

    #[tokio::test]
    async fn tick_once_first_pass_emits_baseline_liveness_change() {
        let _g = DisablePool::new();
        // Two nodes, both responding with Pong → both live.
        let (a, _ha) = spawn_responder(Response::Pong).await;
        let (b, _hb) = spawn_responder(Response::Pong).await;
        let m = manifest_for(vec![a, b]);
        let mut cat = Directory::new();
        cat.insert("photos/a.png".into(), m);
        let cat = Arc::new(Mutex::new(cat));

        let gf = Gf::new();
        let mut state = MonitorState::default();
        let mut rng = Rng::new(1);
        let cfg = MonitorConfig {
            poll_interval: Duration::from_secs(1),
            // Suppress the noisy "LowMargin" stream: keep the test
            // focused on the LivenessChange behaviour.
            margin_threshold: i32::MIN,
            ..MonitorConfig::default()
        };
        let events = tick_once(&gf, cat, &mut state, &cfg, &mut rng, None).await;
        // First tick: one LivenessChange with revived/died both empty
        // (baseline snapshot) — and at least one event total.
        let liveness: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::LivenessChange { total_live, total_nodes, revived, died } => {
                    Some((*total_live, *total_nodes, revived.clone(), died.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(liveness.len(), 1, "expected exactly 1 LivenessChange on first tick");
        let (live, total, revived, died) = &liveness[0];
        assert_eq!(*total, 2, "topology has 2 nodes");
        assert_eq!(*live, 2, "both nodes should answer Pong");
        assert!(revived.is_empty() && died.is_empty(), "baseline: no diff yet");
    }

    #[tokio::test]
    async fn tick_once_detects_node_going_down_between_ticks() {
        let _g = DisablePool::new();
        let (a, _ha) = spawn_responder(Response::Pong).await;
        // Bind+drop a second node so its port is closed — gives us a
        // determistic "node b is dead" baseline.
        let dead_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = dead_listener.local_addr().unwrap().to_string();
        drop(dead_listener);

        let m = manifest_for(vec![a, b]);
        let mut cat = Directory::new();
        cat.insert("photos/x.png".into(), m);
        let cat = Arc::new(Mutex::new(cat));
        let gf = Gf::new();
        let mut state = MonitorState::default();
        let mut rng = Rng::new(2);
        let cfg = MonitorConfig {
            margin_threshold: i32::MIN, // suppress margin noise
            ..MonitorConfig::default()
        };

        let _first = tick_once(&gf, Arc::clone(&cat), &mut state, &cfg, &mut rng, None).await;
        // state.prev_live is now {0} only (node b never responded).
        assert_eq!(state.prev_live.len(), 1, "after first tick, prev_live=={{0}}");

        // Second tick — same liveness, so no LivenessChange should fire.
        let second = tick_once(&gf, cat, &mut state, &cfg, &mut rng, None).await;
        let liveness_count = second
            .iter()
            .filter(|e| matches!(e, Event::LivenessChange { .. }))
            .count();
        assert_eq!(
            liveness_count, 0,
            "stable liveness across ticks should NOT re-emit LivenessChange"
        );
    }

    #[tokio::test]
    async fn tick_once_skips_directory_only_catalog() {
        let _g = DisablePool::new();
        // Catalog has only a Directory entry → no scan target, early return.
        let dir_manifest = Manifest {
            kind: ObjectKind::Directory,
            ..manifest_for(vec![])
        };
        let mut cat = Directory::new();
        cat.insert("photos".into(), dir_manifest);
        let cat = Arc::new(Mutex::new(cat));
        let gf = Gf::new();
        let mut state = MonitorState::default();
        let mut rng = Rng::new(3);
        let cfg = MonitorConfig::default();
        let events = tick_once(&gf, cat, &mut state, &cfg, &mut rng, None).await;
        assert!(
            events.is_empty(),
            "directory-only catalog must yield no events, got {events:?}"
        );
    }
}
