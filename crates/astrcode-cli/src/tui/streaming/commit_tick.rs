//! Commit-tick: apply chunking policy to drain queued lines.

use std::time::Instant;

use ratatui::text::Line;

use super::{
    chunking::{AdaptiveChunkingPolicy, DrainPlan},
    controller::StreamController,
};

/// Output of a single commit tick.
pub struct CommitTickOutput {
    pub lines: Vec<Line<'static>>,
    pub all_idle: bool,
}

/// Run one commit tick against the provided stream controller.
pub fn run_commit_tick(
    policy: &mut AdaptiveChunkingPolicy,
    controller: Option<&mut StreamController>,
    now: Instant,
) -> CommitTickOutput {
    let Some(ctrl) = controller else {
        return CommitTickOutput {
            lines: Vec::new(),
            all_idle: true,
        };
    };

    let state = ctrl.state_mut();
    let queued_len = state.queued_len();
    let oldest_age = state.oldest_queued_age(now);
    let plan = policy.decide(queued_len, oldest_age, now);

    let lines = match plan {
        DrainPlan::Single => state.step().into_iter().collect(),
        DrainPlan::Batch(n) => state.drain_n(n),
    };

    let all_idle = state.is_idle();
    CommitTickOutput { lines, all_idle }
}
