use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use super::{
    assemble::{
        collapse_compaction_whitespace, parse_compact_summary_message, sanitize_compact_summary,
    },
    is_synthetic_context_message,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactPromptMode {
    Fresh,
    Incremental { previous_summary: String },
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedCompactInput {
    pub messages: Vec<LlmMessage>,
    pub prompt_mode: CompactPromptMode,
}

pub(crate) fn prepare_compact_input(messages: &[LlmMessage]) -> PreparedCompactInput {
    let prompt_mode = latest_previous_summary(messages)
        .map(|previous_summary| CompactPromptMode::Incremental { previous_summary })
        .unwrap_or(CompactPromptMode::Fresh);
    let messages = messages
        .iter()
        .filter_map(normalize_compaction_message)
        .collect::<Vec<_>>();
    PreparedCompactInput {
        messages,
        prompt_mode,
    }
}

pub(crate) fn visible_message_text(message: &LlmMessage) -> String {
    message
        .content
        .iter()
        .map(|content| match content {
            LlmContent::Text { text } => text.clone(),
            LlmContent::Image { .. } => "[image]".to_string(),
            LlmContent::ToolCall {
                name, arguments, ..
            } => format!("requested tool {name} with {arguments}"),
            LlmContent::ToolResult { content, .. } => content.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn latest_previous_summary(messages: &[LlmMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != LlmRole::User {
            return None;
        }
        message.content.iter().find_map(|content| match content {
            LlmContent::Text { text } => parse_compact_summary_message(text)
                .map(|envelope| sanitize_compact_summary(&envelope.summary)),
            _ => None,
        })
    })
}

fn normalize_compaction_message(message: &LlmMessage) -> Option<LlmMessage> {
    match message.role {
        LlmRole::System => None,
        LlmRole::User if is_synthetic_context_message(message) => None,
        LlmRole::User => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| LlmMessage::user(text))
        },
        LlmRole::Assistant => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| LlmMessage::assistant(text))
        },
        LlmRole::Tool => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| {
                LlmMessage::tool(
                    message.name.clone().unwrap_or_else(|| "tool".to_string()),
                    first_tool_call_id(message).unwrap_or_else(|| "tool-result".to_string()),
                    text,
                    first_tool_is_error(message),
                )
            })
        },
    }
}

fn first_tool_call_id(message: &LlmMessage) -> Option<String> {
    message.content.iter().find_map(|content| match content {
        LlmContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
        _ => None,
    })
}

fn first_tool_is_error(message: &LlmMessage) -> bool {
    message
        .content
        .iter()
        .any(|content| matches!(content, LlmContent::ToolResult { is_error: true, .. }))
}
