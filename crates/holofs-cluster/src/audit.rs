//! Stage 7.1: Proof of Retrievability.
//!
//! The client periodically asks nodes for a specific shard by hash. If the
//! node really stores it, it returns the shard, and the client verifies
//! `sha256(shard) == hash` to count success. Missing shard → fail. Wrong
//! hash (substitution) → fail.
//!
//! This is not a cryptographically complete PoR (a node could in theory proxy
//! the request to a real holder of the same shard), but it covers the practical
//! attack classes: **silent shard deletion after PUT, full disks, lazy storage,
//! and compromised nodes that stop responding**.
//!
//! Pure logic (`tick_once`) is tested separately from the long-running loop
//! (`run_periodic`); state lives with the caller.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::reputation::{Reputation, DEFAULT_THRESHOLD};
use holofs_client::pool;
use holofs_client::ClientError;
use holofs_core::merkle::{shard_hash, Hash};
use holofs_core::rng::Rng;
use holofs_model::fs::Directory;
use holofs_wire::{read_frame, write_frame, Request, Response};

/// How many shards the auditor checks per tick.
pub const DEFAULT_SAMPLES_PER_TICK: u32 = 20;

/// Interval between ticks.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub interval: Duration,
    pub samples_per_tick: u32,
    pub threshold: f32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            samples_per_tick: DEFAULT_SAMPLES_PER_TICK,
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// Node returned a correct shard with the right hash.
    Pass,
    /// Node said "I don't have it" — `AuditResp { shard: None }`.
    MissingShard,
    /// Node returned a shard whose `sha256` did not match — substitution/corruption.
    HashMismatch,
    /// Network error / timeout / connection refused.
    Unreachable,
    /// Node returned something other than `AuditResp`.
    ProtocolError,
}

impl AuditOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, AuditOutcome::Pass)
    }
}

#[derive(Debug, Clone)]
pub enum AuditEvent {
    Result {
        node: usize,
        object: String,
        channel: u8,
        layer: u8,
        outcome: AuditOutcome,
        score_after: f32,
    },
    LowScore {
        node: usize,
        score: f32,
        threshold: f32,
    },
}

/// Ask a node for a specific shard by hash. The node returns either the
/// shard or `None`. The client verifies `sha256(shard) == shard_hash`.
pub async fn audit_shard(
    addr: &str,
    object_id: u64,
    channel: u8,
    layer: u8,
    shard_hash_expected: Hash,
) -> AuditOutcome {
    let req = Request::Audit {
        object_id,
        channel,
        layer,
        shard_hash: shard_hash_expected,
    };
    let mut s = match pool::acquire(addr).await {
        Ok(s) => s,
        Err(_) => return AuditOutcome::Unreachable,
    };
    if write_frame(&mut s, &req.encode()).await.is_err() {
        s.poison();
        return AuditOutcome::Unreachable;
    }
    let buf = match read_frame(&mut s).await {
        Ok(b) => b,
        Err(_) => {
            s.poison();
            return AuditOutcome::Unreachable;
        }
    };
    let resp = match Response::decode(&buf) {
        Ok(r) => r,
        Err(_) => return AuditOutcome::ProtocolError,
    };
    match resp {
        Response::AuditResp { shard: None } => AuditOutcome::MissingShard,
        Response::AuditResp { shard: Some(s) } => {
            if shard_hash(&s) == shard_hash_expected {
                AuditOutcome::Pass
            } else {
                AuditOutcome::HashMismatch
            }
        }
        _ => AuditOutcome::ProtocolError,
    }
}

/// One auditor pass. Samples `samples_per_tick` random
/// (object, channel, layer, shard_hash) tuples from the catalog; for each
/// one picks the node via `manifest.place_shard()`, queries it, and updates
/// its reputation.
pub async fn tick_once(
    catalog: Arc<Mutex<Directory>>,
    reputation: Arc<Mutex<Reputation>>,
    config: &AuditConfig,
    rng: &mut Rng,
) -> Vec<AuditEvent> {
    let mut events = Vec::new();

    // Snapshot the catalog so we do not hold the lock across network RPCs.
    // Directory markers are filtered out — they carry zero nodes and
    // zero shards, so they're never auditable. Including them as the
    // canonical "first entry" used to derive `live_all` would yield an
    // empty live set and panic inside `place_shard`'s zone-aware
    // placement (see holofs-cluster issue tracker; pre-Stage 14.3
    // bug).
    let snapshot: Vec<(String, holofs_model::manifest::Manifest)> = {
        use holofs_model::manifest::ObjectKind;
        let cat = catalog.lock().await;
        cat.entries
            .iter()
            .filter(|(_, m)| m.kind != ObjectKind::Directory && !m.nodes.is_empty())
            .map(|(n, m)| (n.clone(), m.clone()))
            .collect()
    };
    if snapshot.is_empty() {
        return events;
    }

    let live_all: Vec<usize> = (0..snapshot[0].1.nodes.len()).collect();

    for _ in 0..config.samples_per_tick {
        // Pick a random object.
        let obj_idx = (rng.next() as usize) % snapshot.len();
        let (name, manifest) = &snapshot[obj_idx];

        // Random (channel, layer) with at least one shard.
        let c = (rng.next() as usize) % manifest.channels as usize;
        let l = (rng.next() as usize) % manifest.nlayers as usize;
        let hashes = &manifest.shard_hashes[c][l];
        if hashes.is_empty() {
            continue;
        }
        let h_idx = (rng.next() as usize) % hashes.len();
        let target_hash = hashes[h_idx];

        // Find which node should hold this shard.
        // shard_idx is normally the position in shard_hashes (at PUT time).
        // After repairs shard_hashes may contain hashes from other nodes; in
        // that case `place_shard` points to the "canonical" node, and audit
        // there may return Missing — a normal outcome reflecting shard movement.
        let shard_idx = h_idx as u32;
        let node = manifest.place_shard(c as u8, l as u8, shard_idx, &live_all);

        let outcome = audit_shard(
            &manifest.nodes[node],
            manifest.object_id,
            c as u8,
            l as u8,
            target_hash,
        )
        .await;

        // MissingShard is the one ambiguous outcome: "we don't know if the
        // node is at fault". For the prototype we treat it as negative: the
        // hash came from manifest.shard_hashes and the position from
        // place_shard, so if the node did not deliver, something is wrong
        // with its state (full `shard_idx` ↔ `target_hash` correspondence is
        // guaranteed only right after PUT; repaired shards may land elsewhere.
        // Fine for a prototype.)
        let success = outcome.is_success();
        let score_after = {
            let mut rep = reputation.lock().await;
            rep.observe(node, success);
            rep.score(node)
        };

        events.push(AuditEvent::Result {
            node,
            object: name.clone(),
            channel: c as u8,
            layer: l as u8,
            outcome,
            score_after,
        });
        if score_after < config.threshold {
            events.push(AuditEvent::LowScore {
                node,
                score: score_after,
                threshold: config.threshold,
            });
        }
    }
    events
}

/// Long-running auditor loop. Ticks every `interval`; events go to `on_event`.
pub async fn run_periodic(
    catalog: Arc<Mutex<Directory>>,
    reputation: Arc<Mutex<Reputation>>,
    config: AuditConfig,
    on_event: impl Fn(&AuditEvent) + Send + Sync + 'static,
) {
    let mut rng = Rng::new(0xA1D17u64);
    loop {
        let events = tick_once(
            Arc::clone(&catalog),
            Arc::clone(&reputation),
            &config,
            &mut rng,
        )
        .await;
        for e in &events {
            on_event(e);
        }
        tokio::time::sleep(config.interval).await;
    }
}

// Stub so that `ClientError` is not an unused import in case `audit_shard`
// is ever changed to return Result<_, ClientError>.
#[allow(dead_code)]
fn _link_error() -> Option<ClientError> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_sane() {
        let c = AuditConfig::default();
        assert!(c.interval.as_secs() >= 1);
        assert!(c.samples_per_tick > 0);
        assert!(c.threshold > 0.0 && c.threshold <= 1.0);
    }

    #[test]
    fn outcome_is_success_only_for_pass() {
        assert!(AuditOutcome::Pass.is_success());
        for o in [
            AuditOutcome::MissingShard,
            AuditOutcome::HashMismatch,
            AuditOutcome::Unreachable,
            AuditOutcome::ProtocolError,
        ] {
            assert!(!o.is_success());
        }
    }
}
