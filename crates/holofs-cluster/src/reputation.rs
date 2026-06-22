//! Stage 7.2: node reputation.
//!
//! Plain EMA over audit results: every `success/fail` observation is mixed
//! with the old value using factor `alpha`. This yields a "progressively
//! forgetting" estimate — a long tail of audits is automatically displaced
//! by fresh observations.
//!
//! `Reputation` is a pure state table, no IO; updated by the auditor
//! (`crate::audit::run_periodic`) and read by the monitor when filtering
//! the live set.

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
}

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
}
