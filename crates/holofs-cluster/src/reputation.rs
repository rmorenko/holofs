//! Stage 7.2: node reputation.
//!
//! Plain EMA over audit results: every `success/fail` observation is mixed
//! with the old value using factor `alpha`. This yields a "progressively
//! forgetting" estimate — a long tail of audits is automatically displaced
//! by fresh observations.
//!
//! Updated by the auditor (`crate::audit::run_periodic`) and read by the
//! monitor when filtering the live set.
//!
//! N5: `save_atomic` + `load_or_new` persist the score table to disk so a
//! restart doesn't reset every node's history back to the default
//! `initial` score. The file format is deliberately minimal (magic + u32
//! count + f32 scores + f32 alpha) and self-checking — a `n_nodes`
//! mismatch or corrupted body falls back to a fresh table.

use std::fs;
use std::io;
use std::path::Path;

use holofs_client::LiveNodes;

/// Default smoothing factor: smaller = longer memory.
/// 0.2 → the last ~10 observations dominate.
pub const DEFAULT_ALPHA: f32 = 0.2;

/// Default exclusion threshold: nodes with score < 0.5 are dropped from the live set.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone)]
pub struct Reputation {
    /// `scores[i]` — current node `i` reputation ∈ [0..1].
    scores: Vec<f32>,
    alpha: f32,
}

impl Reputation {
    /// New stat with `n_nodes` nodes and starting score `initial`.
    pub fn new(n_nodes: usize, initial: f32) -> Self {
        Self {
            scores: vec![initial.clamp(0.0, 1.0); n_nodes],
            alpha: DEFAULT_ALPHA,
        }
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha.clamp(0.0, 1.0);
        self
    }

    pub fn len(&self) -> usize {
        self.scores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// EMA update: `new = α · outcome + (1-α) · old`.
    pub fn observe(&mut self, node: usize, success: bool) {
        if node >= self.scores.len() {
            return;
        }
        let outcome = if success { 1.0 } else { 0.0 };
        self.scores[node] = self.alpha * outcome + (1.0 - self.alpha) * self.scores[node];
    }

    pub fn score(&self, node: usize) -> f32 {
        self.scores.get(node).copied().unwrap_or(0.0)
    }

    pub fn alive(&self, node: usize, threshold: f32) -> bool {
        self.score(node) >= threshold
    }

    /// Filter the live set to drop nodes with poor reputation.
    pub fn filter_live(&self, live: &LiveNodes, threshold: f32) -> LiveNodes {
        live.iter()
            .copied()
            .filter(|&n| self.alive(n, threshold))
            .collect()
    }

    pub fn scores(&self) -> &[f32] {
        &self.scores
    }

    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    // === N5: on-disk persistence ===========================================

    /// Wire format for the persisted reputation file. All scalars are
    /// little-endian (host order for x86_64/aarch64 alike). Explicit magic
    /// so a corrupt / mismatched file can be recognised on load rather than
    /// silently misinterpreted as random f32 noise.
    ///
    /// Layout:
    ///   [0..8)   b"HOLOFSR1"    (magic)
    ///   [8..12)  u32 n_nodes    (little-endian)
    ///   [12..16) f32 alpha
    ///   [16..16 + 4*n)   f32 scores[i], one per node
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.scores.len() * 4);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.scores.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.alpha.to_le_bytes());
        for s in &self.scores {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Decode a byte string produced by [`Self::encode`]. Enforces the
    /// magic prefix + length match; returns `InvalidData` on any
    /// inconsistency.
    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reputation blob too short"));
        }
        if &buf[..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reputation magic mismatch"));
        }
        let n = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let alpha = f32::from_le_bytes(buf[12..16].try_into().unwrap()).clamp(0.0, 1.0);
        let expected = 16 + n * 4;
        if buf.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reputation length mismatch: expected {expected}, got {}",
                    buf.len()
                ),
            ));
        }
        let mut scores = Vec::with_capacity(n);
        for i in 0..n {
            let base = 16 + i * 4;
            let s = f32::from_le_bytes(buf[base..base + 4].try_into().unwrap()).clamp(0.0, 1.0);
            scores.push(s);
        }
        Ok(Self { scores, alpha })
    }

    /// Atomically persist the score table to `path`. Uses the same
    /// unique-tmp + rename pattern as `Directory::save_atomic` so
    /// concurrent auditor ticks (very unlikely in practice, but
    /// defensively supported) can't collide on the shared tmp name.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> io::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = match path.file_name() {
            Some(name) => {
                let mut fname = name.to_os_string();
                fname.push(format!(".tmp.{pid}.{n}"));
                path.with_file_name(fname)
            }
            None => path.with_extension(format!("tmp.{pid}.{n}")),
        };
        fs::write(&tmp, self.encode())?;
        if let Ok(f) = fs::File::open(&tmp) {
            let _ = f.sync_all();
        }
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Load the reputation table from `path`, or fall back to a fresh
    /// `Reputation::new(n_nodes, initial)` on any of: missing file,
    /// corrupt magic, size mismatch, or `n_nodes` differing from the
    /// stored table. A `n_nodes` mismatch always drops the persisted
    /// history because the score indices no longer align with the
    /// current cluster topology (the operator added / removed nodes
    /// since the last shutdown).
    ///
    /// Returns `(reputation, loaded)` — `loaded=true` means the state
    /// came from disk; the caller logs that at INFO.
    pub fn load_or_new(
        path: impl AsRef<Path>,
        n_nodes: usize,
        initial: f32,
    ) -> (Self, bool) {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => return (Self::new(n_nodes, initial), false),
        };
        match Self::decode(&bytes) {
            Ok(r) if r.scores.len() == n_nodes => (r, true),
            Ok(r) => {
                // n_nodes changed — the historical scores no longer
                // apply. Log via caller (this fn has no logger) and
                // seed fresh.
                eprintln!(
                    "reputation: on-disk size {} != n_nodes {}, discarding history",
                    r.scores.len(),
                    n_nodes
                );
                (Self::new(n_nodes, initial), false)
            }
            Err(e) => {
                eprintln!("reputation: on-disk load failed ({e}), discarding history");
                (Self::new(n_nodes, initial), false)
            }
        }
    }
}

const MAGIC: &[u8] = b"HOLOFSR1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_moves_toward_outcome() {
        let mut r = Reputation::new(3, 1.0);
        // Drive a stream of failures — score drops.
        for _ in 0..50 {
            r.observe(0, false);
        }
        assert!(r.score(0) < 0.01);
        // Drive a stream of successes on another node — score stays high.
        for _ in 0..50 {
            r.observe(1, true);
        }
        assert!(r.score(1) > 0.99);
    }

    #[test]
    fn out_of_bounds_observe_is_noop() {
        let mut r = Reputation::new(2, 1.0);
        r.observe(99, false); // must not panic
        assert_eq!(r.score(0), 1.0);
        assert_eq!(r.score(1), 1.0);
    }

    #[test]
    fn alive_threshold_works() {
        let mut r = Reputation::new(2, 0.9);
        assert!(r.alive(0, 0.5));
        for _ in 0..20 {
            r.observe(0, false);
        }
        assert!(!r.alive(0, 0.5));
    }

    #[test]
    fn filter_live_drops_low_score() {
        let mut r = Reputation::new(4, 1.0);
        for _ in 0..30 {
            r.observe(1, false);
            r.observe(3, false);
        }
        let live = vec![0, 1, 2, 3];
        let filtered = r.filter_live(&live, 0.5);
        assert_eq!(filtered, vec![0, 2]);
    }

    #[test]
    fn alpha_controls_memory_length() {
        let mut fast = Reputation::new(1, 1.0).with_alpha(0.5);
        let mut slow = Reputation::new(1, 1.0).with_alpha(0.05);
        for _ in 0..3 {
            fast.observe(0, false);
            slow.observe(0, false);
        }
        // Fast forgets quicker → lower score after the same 3 failures.
        assert!(fast.score(0) < slow.score(0));
    }

    #[test]
    fn new_clamps_initial_into_unit_interval() {
        let r_low = Reputation::new(3, -0.5);
        let r_high = Reputation::new(3, 1.5);
        assert_eq!(r_low.score(0), 0.0);
        assert_eq!(r_high.score(0), 1.0);
    }

    #[test]
    fn with_alpha_clamps_outside_unit_interval() {
        // The factor must stay in [0, 1]; out-of-range values get
        // clamped silently so the EMA stays a contraction.
        let r_low = Reputation::new(1, 0.5).with_alpha(-0.2);
        let r_high = Reputation::new(1, 0.5).with_alpha(2.0);
        // α = 0 → observe is a no-op
        let mut a = r_low.clone();
        a.observe(0, true);
        assert!((a.score(0) - 0.5).abs() < 1e-6);
        // α = 1 → observe snaps to the outcome
        let mut b = r_high.clone();
        b.observe(0, true);
        assert!((b.score(0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn score_on_out_of_bounds_node_returns_zero() {
        let r = Reputation::new(2, 1.0);
        assert_eq!(r.score(99), 0.0);
        // `alive` for a missing node returns false at any positive threshold.
        assert!(!r.alive(99, 0.5));
    }

    // === N5: persistence unit tests =======================================

    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "holofs-reputation-{tag}-{}-{now}",
            std::process::id()
        ))
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut r = Reputation::new(4, 0.75).with_alpha(0.3);
        r.observe(0, false);
        r.observe(2, true);
        let bytes = r.encode();
        let back = Reputation::decode(&bytes).unwrap();
        assert_eq!(back.len(), r.len());
        assert!((back.alpha() - 0.3).abs() < 1e-6);
        for i in 0..r.len() {
            assert!((back.score(i) - r.score(i)).abs() < 1e-6);
        }
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = Reputation::new(2, 0.5).encode();
        bytes[0] = b'X';
        assert!(Reputation::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        let mut bytes = Reputation::new(2, 0.5).encode();
        bytes.pop();
        assert!(Reputation::decode(&bytes).is_err());
    }

    #[test]
    fn save_atomic_roundtrip() {
        let dir = scratch_path("save-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reputation.bin");
        let mut r = Reputation::new(3, 0.9);
        for _ in 0..5 {
            r.observe(1, false);
        }
        r.save_atomic(&path).unwrap();
        let (back, loaded) = Reputation::load_or_new(&path, 3, 1.0);
        assert!(loaded, "load_or_new did not signal 'loaded from disk'");
        for i in 0..3 {
            assert!((back.score(i) - r.score(i)).abs() < 1e-6);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_new_missing_file_returns_fresh() {
        let path = scratch_path("missing").join("nope.bin");
        let (r, loaded) = Reputation::load_or_new(&path, 4, 0.7);
        assert!(!loaded);
        for i in 0..4 {
            assert!((r.score(i) - 0.7).abs() < 1e-6);
        }
    }

    #[test]
    fn load_or_new_size_mismatch_drops_history() {
        let dir = scratch_path("size-mismatch");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reputation.bin");
        // Persisted with 3 slots.
        Reputation::new(3, 0.5).save_atomic(&path).unwrap();
        // Loading with 5 nodes must discard and seed fresh.
        let (r, loaded) = Reputation::load_or_new(&path, 5, 0.9);
        assert!(!loaded, "loaded=true despite size mismatch");
        assert_eq!(r.len(), 5);
        for i in 0..5 {
            assert!((r.score(i) - 0.9).abs() < 1e-6);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn len_and_is_empty_reflect_constructor_size() {
        let r0 = Reputation::new(0, 0.5);
        assert_eq!(r0.len(), 0);
        assert!(r0.is_empty());
        let r3 = Reputation::new(3, 0.5);
        assert_eq!(r3.len(), 3);
        assert!(!r3.is_empty());
        assert_eq!(r3.scores().len(), 3);
    }
}
