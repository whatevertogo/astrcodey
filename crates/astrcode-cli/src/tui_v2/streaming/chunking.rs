//! Adaptive stream chunking policy.
//!
//! Two-gear system ported from codex-rs `tui/src/streaming/chunking.rs`:
//! - Smooth: drain 1 line per commit-tick (steady baseline pacing)
//! - CatchUp: drain all queued lines (converge backlog immediately)
//!
//! Hysteresis prevents rapid gear-flapping near thresholds.

use std::time::{Duration, Instant};

// ─── Thresholds (tuned from codex defaults) ───────────────────────────────────

/// Enter CatchUp when queue depth ≥ this.
const ENTER_QUEUE_DEPTH: usize = 8;
/// Enter CatchUp when oldest queued line age ≥ this.
const ENTER_OLDEST_AGE: Duration = Duration::from_millis(300);
/// Exit CatchUp when queue depth ≤ this.
const EXIT_QUEUE_DEPTH: usize = 2;
/// Exit CatchUp when oldest age ≤ this.
const EXIT_OLDEST_AGE: Duration = Duration::from_millis(100);
/// Hold exit condition for this long before actually exiting.
const EXIT_HOLD: Duration = Duration::from_millis(150);
/// After exiting CatchUp, suppress re-entry for this long (unless severe).
const REENTER_HOLD: Duration = Duration::from_millis(500);
/// Severe queue depth — bypass re-entry hold.
const SEVERE_QUEUE_DEPTH: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkingMode {
    Smooth,
    CatchUp,
}

#[derive(Debug, Clone, Copy)]
pub enum DrainPlan {
    /// Drain exactly one line.
    Single,
    /// Drain all currently queued lines.
    Batch(usize),
}

pub struct AdaptiveChunkingPolicy {
    mode: ChunkingMode,
    exit_hold_since: Option<Instant>,
    reenter_hold_since: Option<Instant>,
}

impl AdaptiveChunkingPolicy {
    pub fn new() -> Self {
        Self {
            mode: ChunkingMode::Smooth,
            exit_hold_since: None,
            reenter_hold_since: None,
        }
    }

    pub fn mode(&self) -> ChunkingMode {
        self.mode
    }

    /// Decide how many lines to drain given current queue state.
    pub fn decide(
        &mut self,
        queued_len: usize,
        oldest_age: Option<Duration>,
        now: Instant,
    ) -> DrainPlan {
        if queued_len == 0 {
            self.mode = ChunkingMode::Smooth;
            self.exit_hold_since = None;
            return DrainPlan::Single;
        }

        match self.mode {
            ChunkingMode::Smooth => {
                if self.should_enter_catch_up(queued_len, oldest_age, now) {
                    self.mode = ChunkingMode::CatchUp;
                    self.exit_hold_since = None;
                    self.reenter_hold_since = None;
                    DrainPlan::Batch(queued_len)
                } else {
                    DrainPlan::Single
                }
            },
            ChunkingMode::CatchUp => {
                if self.should_exit_catch_up(queued_len, oldest_age, now) {
                    self.mode = ChunkingMode::Smooth;
                    self.reenter_hold_since = Some(now);
                    self.exit_hold_since = None;
                    DrainPlan::Single
                } else {
                    DrainPlan::Batch(queued_len)
                }
            },
        }
    }

    fn should_enter_catch_up(
        &mut self,
        queued_len: usize,
        oldest_age: Option<Duration>,
        now: Instant,
    ) -> bool {
        // Bypass re-entry hold for severe backlog.
        if queued_len >= SEVERE_QUEUE_DEPTH {
            return true;
        }
        // Respect re-entry hold.
        if let Some(hold_since) = self.reenter_hold_since {
            if now.duration_since(hold_since) < REENTER_HOLD {
                return false;
            }
            self.reenter_hold_since = None;
        }
        let age_exceeded = oldest_age.is_some_and(|a| a >= ENTER_OLDEST_AGE);
        queued_len >= ENTER_QUEUE_DEPTH || age_exceeded
    }

    fn should_exit_catch_up(
        &mut self,
        queued_len: usize,
        oldest_age: Option<Duration>,
        now: Instant,
    ) -> bool {
        let pressure_low =
            queued_len <= EXIT_QUEUE_DEPTH && oldest_age.is_none_or(|a| a <= EXIT_OLDEST_AGE);
        if !pressure_low {
            self.exit_hold_since = None;
            return false;
        }
        match self.exit_hold_since {
            None => {
                self.exit_hold_since = Some(now);
                false
            },
            Some(hold_since) => now.duration_since(hold_since) >= EXIT_HOLD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooth_mode_drains_single_line() {
        let mut policy = AdaptiveChunkingPolicy::new();
        let now = Instant::now();
        let plan = policy.decide(3, Some(Duration::from_millis(10)), now);
        assert!(matches!(plan, DrainPlan::Single));
        assert_eq!(policy.mode(), ChunkingMode::Smooth);
    }

    #[test]
    fn enters_catch_up_on_high_queue_depth() {
        let mut policy = AdaptiveChunkingPolicy::new();
        let now = Instant::now();
        let plan = policy.decide(ENTER_QUEUE_DEPTH, Some(Duration::from_millis(10)), now);
        assert!(matches!(plan, DrainPlan::Batch(_)));
        assert_eq!(policy.mode(), ChunkingMode::CatchUp);
    }

    #[test]
    fn enters_catch_up_on_old_age() {
        let mut policy = AdaptiveChunkingPolicy::new();
        let now = Instant::now();
        let plan = policy.decide(1, Some(ENTER_OLDEST_AGE), now);
        assert!(matches!(plan, DrainPlan::Batch(_)));
    }

    #[test]
    fn empty_queue_resets_to_smooth() {
        let mut policy = AdaptiveChunkingPolicy::new();
        let now = Instant::now();
        // Force into CatchUp.
        policy.decide(SEVERE_QUEUE_DEPTH, None, now);
        assert_eq!(policy.mode(), ChunkingMode::CatchUp);
        // Empty queue resets.
        policy.decide(0, None, now);
        assert_eq!(policy.mode(), ChunkingMode::Smooth);
    }
}
