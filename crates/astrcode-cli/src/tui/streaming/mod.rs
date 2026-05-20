//! Adaptive streaming pipeline.
//!
//! Architecture ported from codex-rs `tui/src/streaming/`.
//! StreamState holds a queue of committed lines; AdaptiveChunkingPolicy
//! decides how many to drain per commit-tick (Smooth=1, CatchUp=all).

pub mod chunking;
pub mod commit_tick;
pub mod controller;

use std::{collections::VecDeque, time::Instant};

use ratatui::text::Line;

/// A single queued line with its enqueue timestamp (for age-based policy).
pub(super) struct QueuedLine {
    line: Line<'static>,
    enqueued_at: Instant,
}

/// In-flight stream state: queued committed lines + seen-delta flag.
pub struct StreamState {
    queued_lines: VecDeque<QueuedLine>,
    pub has_seen_delta: bool,
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            queued_lines: VecDeque::new(),
            has_seen_delta: false,
        }
    }

    pub fn clear(&mut self) {
        self.queued_lines.clear();
        self.has_seen_delta = false;
    }

    pub fn enqueue(&mut self, lines: Vec<Line<'static>>) {
        let now = Instant::now();
        self.queued_lines
            .extend(lines.into_iter().map(|line| QueuedLine {
                line,
                enqueued_at: now,
            }));
    }

    /// Drain one line from the front.
    pub fn step(&mut self) -> Option<Line<'static>> {
        self.queued_lines.pop_front().map(|q| q.line)
    }

    /// Drain up to `max_lines` from the front.
    pub fn drain_n(&mut self, max_lines: usize) -> Vec<Line<'static>> {
        let end = max_lines.min(self.queued_lines.len());
        self.queued_lines.drain(..end).map(|q| q.line).collect()
    }

    pub fn drain_all(&mut self) -> Vec<Line<'static>> {
        self.queued_lines.drain(..).map(|q| q.line).collect()
    }

    pub fn is_idle(&self) -> bool {
        self.queued_lines.is_empty()
    }

    pub fn queued_len(&self) -> usize {
        self.queued_lines.len()
    }

    pub fn oldest_queued_age(&self, now: Instant) -> Option<std::time::Duration> {
        self.queued_lines
            .front()
            .map(|q| now.saturating_duration_since(q.enqueued_at))
    }
}
