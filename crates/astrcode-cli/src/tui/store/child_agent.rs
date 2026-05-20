//! Child agent output tracker.
//!
//! Parses "child " prefixed structured events from ToolOutputDelta,
//! flushes each meaningful line to scrollback immediately.

use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

#[derive(Debug, Clone, Default)]
pub struct ChildAgentTracker;

impl ChildAgentTracker {
    pub fn handle_delta(&mut self, delta: &str, scrollback_queue: &mut Vec<ScrollbackEntry>) {
        for line in delta.lines() {
            let clean = line.strip_prefix("child ").unwrap_or(line);
            let trimmed = clean.trim();

            if trimmed.is_empty()
                || trimmed == "assistant started"
                || trimmed.starts_with("assistant completed:")
                || trimmed.starts_with("turn completed:")
                || trimmed.starts_with("tool output:")
            {
                continue;
            }

            if let Some(tool_name) = trimmed.strip_prefix("tool started: ") {
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Tool,
                    text: format!("  → {tool_name}"),
                });
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("tool completed: ") {
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Tool,
                    text: format!("  ✓ {rest}"),
                });
                continue;
            }

            if let Some(msg) = trimmed.strip_prefix("error: ") {
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Tool,
                    text: format!("  ! {msg}"),
                });
                continue;
            }
        }
    }

    pub fn flush_on_completion(&mut self, scrollback_queue: &mut Vec<ScrollbackEntry>) {
        scrollback_queue.push(ScrollbackEntry::BlankLine);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_agent_flushes_tools_immediately() {
        let mut tracker = ChildAgentTracker;
        let mut queue = Vec::new();

        tracker.handle_delta("child assistant started\n", &mut queue);
        tracker.handle_delta("child tool started: find\n", &mut queue);
        tracker.handle_delta("child tool completed: find: 3 files\n", &mut queue);

        assert_eq!(queue.len(), 2);
        assert!(
            matches!(&queue[0], ScrollbackEntry::StreamText { text, .. } if text.contains("→ find"))
        );
        assert!(
            matches!(&queue[1], ScrollbackEntry::StreamText { text, .. } if text.contains("✓ find: 3 files"))
        );
    }

    #[test]
    fn child_agent_discards_assistant_text() {
        let mut tracker = ChildAgentTracker;
        let mut queue = Vec::new();

        tracker.handle_delta("child assistant started\n", &mut queue);
        tracker.handle_delta("child thinking about the problem...\n", &mut queue);
        tracker.handle_delta("child assistant completed: done\n", &mut queue);

        assert!(queue.is_empty());
    }

    #[test]
    fn child_agent_errors_still_show() {
        let mut tracker = ChildAgentTracker;
        let mut queue = Vec::new();

        tracker.handle_delta("child error: something went wrong\n", &mut queue);

        assert_eq!(queue.len(), 1);
        let text = match &queue[0] {
            ScrollbackEntry::StreamText { text, .. } => text.as_str(),
            _ => panic!("expected StreamText"),
        };
        assert!(text.contains("! something went wrong"));
    }
}
