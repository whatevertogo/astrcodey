use std::collections::HashSet;

use astrcode_core::{LlmMessage, UserMessageOrigin};

use super::tool_results::tool_call_name_map;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub truncated_tool_results: usize,
    pub cleared_tool_results: usize,
}

#[derive(Debug, Clone)]
pub struct PruneOutcome {
    pub messages: Vec<LlmMessage>,
    pub stats: PruneStats,
}

pub fn apply_prune_pass(
    messages: &[LlmMessage],
    clearable_tools: &HashSet<String>,
    max_tool_result_bytes: usize,
    keep_recent_turns: usize,
) -> PruneOutcome {
    let tool_call_names = tool_call_name_map(messages);
    let keep_start = recent_turn_start_index(messages, keep_recent_turns.max(1));
    let mut truncated_tool_results = 0usize;
    let mut cleared_tool_results = 0usize;
    let mut compacted = messages.to_vec();

    for (index, message) in compacted.iter_mut().enumerate() {
        let LlmMessage::Tool {
            tool_call_id,
            content,
        } = message
        else {
            continue;
        };

        if content.len() > max_tool_result_bytes {
            *content = truncate_tool_content(content, max_tool_result_bytes);
            truncated_tool_results += 1;
        }

        if index >= keep_start {
            continue;
        }

        let Some(tool_name) = tool_call_names.get(tool_call_id) else {
            continue;
        };
        if clearable_tools.contains(tool_name) {
            *content = format!(
                "[cleared older tool result from '{tool_name}' to reduce prompt size; reload it \
                 if needed]"
            );
            cleared_tool_results += 1;
        }
    }

    PruneOutcome {
        messages: compacted,
        stats: PruneStats {
            truncated_tool_results,
            cleared_tool_results,
        },
    }
}

fn truncate_tool_content(content: &str, max_bytes: usize) -> String {
    let total_bytes = content.len();
    let mut visible_bytes = max_bytes.saturating_sub(96).max(64).min(total_bytes);
    while !content.is_char_boundary(visible_bytes) {
        visible_bytes = visible_bytes.saturating_sub(1);
    }
    let visible = &content[..visible_bytes];
    format!(
        "[truncated: original {total_bytes} bytes, showing first {visible_bytes} bytes]\n{visible}"
    )
}

fn recent_turn_start_index(messages: &[LlmMessage], requested_recent_turns: usize) -> usize {
    let user_turn_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            LlmMessage::User {
                origin: UserMessageOrigin::User,
                ..
            } => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    if user_turn_indices.is_empty() {
        return messages.len();
    }

    let keep_turns = requested_recent_turns.min(user_turn_indices.len()).max(1);
    user_turn_indices[user_turn_indices.len() - keep_turns]
}
