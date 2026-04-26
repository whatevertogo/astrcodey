use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

use astrcode_core::LlmMessage;
use chrono::{DateTime, Utc};

use super::tool_results::tool_call_name_map;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicroCompactConfig {
    pub gap_threshold: Duration,
    pub keep_recent_results: usize,
}

#[derive(Debug, Clone)]
pub struct MicroCompactOutcome {
    pub messages: Vec<LlmMessage>,
}

#[derive(Debug, Clone)]
struct TrackedToolResult {
    tool_call_id: String,
    recorded_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct MicroCompactState {
    tracked_results: VecDeque<TrackedToolResult>,
    last_prompt_activity: Option<Instant>,
}

impl MicroCompactState {
    pub fn seed_from_messages(
        messages: &[LlmMessage],
        config: MicroCompactConfig,
        now: Instant,
        last_assistant_at: Option<DateTime<Utc>>,
    ) -> Self {
        let mut state = Self::default();
        let stale_at = now.checked_sub(config.gap_threshold).unwrap_or(now);
        let restored_activity = last_assistant_at
            .and_then(|timestamp| instant_from_timestamp(now, timestamp))
            .unwrap_or(stale_at);

        for message in messages {
            match message {
                LlmMessage::Assistant { .. } | LlmMessage::Tool { .. } => {
                    state.last_prompt_activity = Some(restored_activity);
                },
                _ => {},
            }

            let LlmMessage::Tool { tool_call_id, .. } = message else {
                continue;
            };
            state.tracked_results.push_back(TrackedToolResult {
                tool_call_id: tool_call_id.clone(),
                recorded_at: stale_at,
            });
        }

        state
    }

    pub fn record_tool_result(&mut self, tool_call_id: impl Into<String>, now: Instant) {
        let tool_call_id = tool_call_id.into();
        self.tracked_results
            .retain(|entry| entry.tool_call_id != tool_call_id);
        self.tracked_results.push_back(TrackedToolResult {
            tool_call_id,
            recorded_at: now,
        });
        self.last_prompt_activity = Some(now);
    }

    pub fn record_assistant_activity(&mut self, now: Instant) {
        self.last_prompt_activity = Some(now);
    }

    pub fn apply_if_idle(
        &mut self,
        messages: &[LlmMessage],
        clearable_tools: &HashSet<String>,
        config: MicroCompactConfig,
        now: Instant,
    ) -> MicroCompactOutcome {
        self.retain_live_tool_results(messages);

        let Some(last_activity) = self.last_prompt_activity else {
            return MicroCompactOutcome {
                messages: messages.to_vec(),
            };
        };

        if now.duration_since(last_activity) < config.gap_threshold {
            return MicroCompactOutcome {
                messages: messages.to_vec(),
            };
        }

        let keep_recent_results = config.keep_recent_results.max(1);
        if self.tracked_results.len() <= keep_recent_results {
            return MicroCompactOutcome {
                messages: messages.to_vec(),
            };
        }

        let tool_call_names = tool_call_name_map(messages);
        let protected_ids = self
            .tracked_results
            .iter()
            .rev()
            .take(keep_recent_results)
            .map(|entry| entry.tool_call_id.as_str())
            .collect::<HashSet<_>>();

        let stale_ids = self
            .tracked_results
            .iter()
            .filter(|entry| !protected_ids.contains(entry.tool_call_id.as_str()))
            .filter(|entry| now.duration_since(entry.recorded_at) >= config.gap_threshold)
            .filter_map(|entry| {
                tool_call_names
                    .get(&entry.tool_call_id)
                    .filter(|tool_name| clearable_tools.contains(*tool_name))
                    .map(|_| entry.tool_call_id.clone())
            })
            .collect::<HashSet<_>>();

        if stale_ids.is_empty() {
            return MicroCompactOutcome {
                messages: messages.to_vec(),
            };
        }

        let mut compacted = messages.to_vec();
        for message in &mut compacted {
            let LlmMessage::Tool {
                tool_call_id,
                content,
            } = message
            else {
                continue;
            };

            if !stale_ids.contains(tool_call_id) || is_micro_compacted(content) {
                continue;
            }

            let tool_name = tool_call_names
                .get(tool_call_id)
                .map(String::as_str)
                .unwrap_or("tool");
            *content = format!(
                "[micro-compacted stale tool result from '{tool_name}' after idle gap; rerun the \
                 tool if exact output is needed]"
            );
        }

        MicroCompactOutcome {
            messages: compacted,
        }
    }

    fn retain_live_tool_results(&mut self, messages: &[LlmMessage]) {
        let live_tool_ids = messages
            .iter()
            .filter_map(|message| match message {
                LlmMessage::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        self.tracked_results
            .retain(|entry| live_tool_ids.contains(entry.tool_call_id.as_str()));
    }
}

fn is_micro_compacted(content: &str) -> bool {
    content.contains("[micro-compacted stale tool result")
}

fn instant_from_timestamp(now: Instant, timestamp: DateTime<Utc>) -> Option<Instant> {
    let elapsed = (Utc::now() - timestamp).to_std().ok()?;
    now.checked_sub(elapsed).or(Some(now))
}
