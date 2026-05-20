//! Child agent output tracker.
//!
//! Parses "child " prefixed structured events from ToolOutputDelta,
//! accumulates assistant text, and emits compact scrollback entries.

use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

#[derive(Debug, Clone, Default)]
pub struct ChildAgentTracker {
    pub completed_tools: Vec<String>,
    pub running_tools: Vec<String>,
    pub pending_output: String,
}

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

            if trimmed.starts_with("tool started: ")
                || trimmed.starts_with("tool completed: ")
                || trimmed.starts_with("error: ")
            {
                Self::flush_pending(&mut self.pending_output, scrollback_queue);
            }

            if let Some(tool_name) = trimmed.strip_prefix("tool started: ") {
                self.running_tools.push(tool_name.to_string());
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Tool,
                    text: format!("  · {tool_name}"),
                });
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("tool completed: ") {
                let tool_name = rest.split(':').next().unwrap_or(rest).trim();
                self.completed_tools.push(tool_name.to_string());
                self.running_tools.retain(|t| t != tool_name);
                continue;
            }

            if let Some(msg) = trimmed.strip_prefix("error: ") {
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Tool,
                    text: format!("  ! {msg}"),
                });
                continue;
            }

            self.pending_output.push_str(clean);
            if self.pending_output.len() >= 200 {
                let text = std::mem::take(&mut self.pending_output);
                scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Assistant,
                    text,
                });
            }
        }
    }

    pub fn flush_on_completion(&mut self, scrollback_queue: &mut Vec<ScrollbackEntry>) {
        Self::flush_pending(&mut self.pending_output, scrollback_queue);
        if !self.completed_tools.is_empty() {
            scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Tool,
                text: format!(
                    "  {} tool(s): {}",
                    self.completed_tools.len(),
                    completed_tools_summary(&self.completed_tools).join(", ")
                ),
            });
        }
        scrollback_queue.push(ScrollbackEntry::BlankLine);
    }

    fn flush_pending(pending: &mut String, scrollback_queue: &mut Vec<ScrollbackEntry>) {
        let text = std::mem::take(pending);
        if !text.is_empty() {
            scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Assistant,
                text,
            });
        }
    }
}

fn completed_tools_summary(completed_tools: &[String]) -> Vec<String> {
    use std::collections::BTreeMap;
    completed_tools
        .iter()
        .fold(BTreeMap::<&String, usize>::new(), |mut acc, tool| {
            *acc.entry(tool).or_default() += 1;
            acc
        })
        .into_iter()
        .map(|(name, count)| {
            if count > 1 {
                format!("{name}({count})")
            } else {
                name.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_agent_accumulates_text_and_shows_compact_tools() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        tracker.handle_delta("child assistant started\n", &mut queue);
        tracker.handle_delta("child 我来系统地探索项目中的设计。\n", &mut queue);
        tracker.handle_delta("child tool started: find\n", &mut queue);
        tracker.handle_delta("child tool completed: find: 3 files\n", &mut queue);
        tracker.handle_delta("child tool started: read\n", &mut queue);
        tracker.handle_delta("child assistant completed: 找到了相关文件\n", &mut queue);

        let stream_texts: Vec<&str> = queue
            .iter()
            .filter_map(|e| match e {
                ScrollbackEntry::StreamText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(stream_texts.len(), 3);
        assert_eq!(stream_texts[0], "我来系统地探索项目中的设计。");
        assert_eq!(stream_texts[1], "  · find");
        assert_eq!(stream_texts[2], "  · read");
        assert!(!stream_texts.iter().any(|t| t.contains("assistant")));
        assert!(!stream_texts.iter().any(|t| t.contains("tool completed")));
    }

    #[test]
    fn child_agent_tool_summary_on_completion() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        tracker.handle_delta(
            "child tool started: find\nchild tool completed: find: ok\nchild tool started: \
             find\nchild tool completed: find: ok\nchild tool started: grep\nchild tool \
             completed: grep: 5 matches\n",
            &mut queue,
        );
        tracker.flush_on_completion(&mut queue);

        let summary = queue.iter().find(
            |e| matches!(e, ScrollbackEntry::StreamText { text, .. } if text.contains("tool(s):")),
        );
        assert!(summary.is_some(), "should have tool summary");
        let text = match summary.unwrap() {
            ScrollbackEntry::StreamText { text, .. } => text.as_str(),
            _ => unreachable!(),
        };
        assert!(text.contains("3 tool(s)"));
        assert!(text.contains("find(2)"));
        assert!(text.contains("grep"));
    }
}
