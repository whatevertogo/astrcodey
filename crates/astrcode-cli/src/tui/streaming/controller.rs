//! StreamController: push_delta → newline-complete → render → enqueue.

use ratatui::text::Line;

use super::StreamState;
use crate::tui::{
    render::{render_spec_to_lines, visual_lines},
    theme::Theme,
};

/// Manages one assistant message stream.
pub struct StreamController {
    state: StreamState,
    /// Accumulated raw text waiting for a newline boundary.
    pending: String,
    /// Width used for rendering (None = no wrapping).
    width: Option<usize>,
}

impl StreamController {
    pub fn new(width: Option<usize>) -> Self {
        Self {
            state: StreamState::new(),
            pending: String::new(),
            width,
        }
    }

    /// Push a text delta. Returns true if new lines were enqueued.
    pub fn push_delta(&mut self, delta: &str, theme: &Theme) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.state.has_seen_delta = true;
        self.pending.push_str(delta);

        if delta.contains('\n') {
            return self.commit_complete_lines(theme);
        }
        false
    }

    /// Finalize the stream: render remaining pending text and return all queued lines.
    pub fn finalize(&mut self, completed_text: &str, theme: &Theme) -> Vec<Line<'static>> {
        if !self.state.has_seen_delta {
            // No deltas seen — render the completed text directly.
            self.pending.push_str(completed_text);
        }
        // Render whatever is left.
        if !self.pending.trim().is_empty() {
            let lines = self.render_text(&self.pending.clone(), theme);
            self.state.enqueue(lines);
        }
        self.pending.clear();
        self.state.drain_all()
    }

    pub fn state_mut(&mut self) -> &mut StreamState {
        &mut self.state
    }

    fn commit_complete_lines(&mut self, theme: &Theme) -> bool {
        // Split on newlines, keep the last (possibly incomplete) chunk in pending.
        let text = std::mem::take(&mut self.pending);
        let mut parts: Vec<&str> = text.splitn(usize::MAX, '\n').collect();
        let last = parts.pop().unwrap_or_default().to_string();
        let complete = parts.join("\n");
        self.pending = last;

        if complete.is_empty() {
            return false;
        }
        let lines = self.render_text(&complete, theme);
        if lines.is_empty() {
            return false;
        }
        self.state.enqueue(lines);
        true
    }

    fn render_text(&self, text: &str, _theme: &Theme) -> Vec<Line<'static>> {
        let width = self.width.unwrap_or(120);
        visual_lines(text, width)
            .into_iter()
            .map(Line::from)
            .collect()
    }
}
