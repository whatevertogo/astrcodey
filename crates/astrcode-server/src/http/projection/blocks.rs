//! 把 LLM message 历史与 EventPayload 投影成 ConversationBlockDto。

use std::collections::BTreeMap;

use astrcode_core::{
    event::{Event, EventPayload},
    llm::{LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, attachments_from_user_message},
    storage::{CompactBoundaryView, SequencedLlmMessage},
};
use astrcode_protocol::http::{ConversationBlockDto, ConversationBlockStatusDto};

use super::args::format_args_inline;

/// 对话 UI 中 compact 卡片的稳定 block id（多次 compact 时 upsert / 刷新替换）。
pub(in crate::http) const COMPACT_SUMMARY_BLOCK_ID: &str = "compact-current";

/// 仅用于对话展示：返回最近一次 compact boundary。
pub(in crate::http) fn latest_compact_boundary(
    boundaries: &[CompactBoundaryView],
) -> Option<&CompactBoundaryView> {
    boundaries.iter().max_by_key(|boundary| boundary.seq)
}

/// 将 compact boundary 投影为对话 block（插在保留消息之前）。
pub(in crate::http) fn compact_summary_block(
    boundary: &CompactBoundaryView,
) -> ConversationBlockDto {
    ConversationBlockDto::CompactSummary {
        id: COMPACT_SUMMARY_BLOCK_ID.to_string(),
        summary: boundary.summary.clone(),
        trigger: boundary.trigger.clone(),
        pre_tokens: boundary.pre_tokens,
        post_tokens: boundary.post_tokens,
        transcript_path: boundary.transcript_path.clone(),
    }
}

/// Build the completed [`ConversationBlockDto`] for payloads that produce a single
/// final block. Shared by live and replay delta functions.
pub(in crate::http) fn completed_block_from_payload(event: &Event) -> Option<ConversationBlockDto> {
    match &event.payload {
        EventPayload::UserMessage {
            message_id,
            text,
            attachments,
            ..
        } => Some(ConversationBlockDto::User {
            id: message_id.to_string(),
            text: text.clone(),
            attachments: attachments.clone(),
            source: None,
        }),
        EventPayload::AssistantMessageCompleted {
            message_id,
            text,
            reasoning_content,
        } => Some(ConversationBlockDto::Assistant {
            id: message_id.to_string(),
            text: text.clone(),
            reasoning_content: reasoning_content.clone(),
            status: ConversationBlockStatusDto::Complete,
        }),
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            arguments,
            arguments_json,
            ..
        } => {
            let metadata: serde_json::Value = serde_json::to_value(&result.metadata)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let metadata = if metadata.as_object().is_some_and(|m| !m.is_empty()) {
                Some(metadata)
            } else {
                None
            };
            Some(ConversationBlockDto::ToolCall {
                id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
                text: result.content.clone(),
                status: if result.is_error {
                    ConversationBlockStatusDto::Error
                } else {
                    ConversationBlockStatusDto::Complete
                },
                metadata,
                arguments_json: arguments_json.clone(),
            })
        },
        EventPayload::ErrorOccurred { message, .. } => Some(ConversationBlockDto::Error {
            id: event.id.to_string(),
            message: message.clone(),
        }),
        EventPayload::CompactBoundaryCreated {
            trigger,
            pre_tokens,
            post_tokens,
            summary,
            transcript_path,
            ..
        } => Some(ConversationBlockDto::CompactSummary {
            id: COMPACT_SUMMARY_BLOCK_ID.to_string(),
            summary: summary.clone(),
            trigger: trigger.clone(),
            pre_tokens: *pre_tokens,
            post_tokens: *post_tokens,
            transcript_path: transcript_path.clone(),
        }),
        EventPayload::RecapGenerated { text, .. } => Some(ConversationBlockDto::SystemNote {
            id: event.id.to_string(),
            text: text.clone(),
        }),
        _ => None,
    }
}

pub(in crate::http) fn messages_to_blocks(
    messages: &[SequencedLlmMessage],
) -> Vec<ConversationBlockDto> {
    let mut blocks = Vec::new();
    let mut tool_block_indices = BTreeMap::new();

    for (index, seq_msg) in messages.iter().enumerate() {
        let message = &seq_msg.message;
        let source = &seq_msg.source;
        if source.as_deref() == Some(TURN_ABORTED_SOURCE) {
            continue;
        }
        let id = format!("snapshot-message-{index}");
        match message.role {
            LlmRole::User => blocks.push(ConversationBlockDto::User {
                id,
                text: visible_message_text(message),
                attachments: attachments_from_user_message(message),
                source: source.clone(),
            }),
            LlmRole::Assistant => {
                let text = visible_message_text(message);
                if !text.trim().is_empty() || message.reasoning_content.is_some() {
                    blocks.push(ConversationBlockDto::Assistant {
                        id,
                        text,
                        reasoning_content: message.reasoning_content.clone(),
                        status: ConversationBlockStatusDto::Complete,
                    });
                }
                for content in &message.content {
                    let LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                    } = content
                    else {
                        continue;
                    };
                    let block_index = blocks.len();
                    blocks.push(ConversationBlockDto::ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: format_args_inline(name, arguments),
                        text: String::new(),
                        status: ConversationBlockStatusDto::Streaming,
                        metadata: None,
                        arguments_json: Some(arguments.clone()),
                    });
                    tool_block_indices.insert(call_id.clone(), block_index);
                }
            },
            LlmRole::Tool => {
                push_tool_result_block(&mut blocks, &tool_block_indices, message, id);
            },
            LlmRole::System => blocks.push(ConversationBlockDto::SystemNote {
                id,
                text: visible_message_text(message),
            }),
        }
    }

    blocks
}

fn push_tool_result_block(
    blocks: &mut Vec<ConversationBlockDto>,
    tool_block_indices: &BTreeMap<String, usize>,
    message: &LlmMessage,
    fallback_id: String,
) {
    let fallback_name = message.name.clone().unwrap_or_else(|| "tool".into());
    let mut pushed_result = false;

    for content in &message.content {
        let LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = content
        else {
            continue;
        };
        let status = if *is_error {
            ConversationBlockStatusDto::Error
        } else {
            ConversationBlockStatusDto::Complete
        };
        if let Some(block_index) = tool_block_indices.get(tool_call_id) {
            if let Some(ConversationBlockDto::ToolCall {
                text,
                status: block_status,
                ..
            }) = blocks.get_mut(*block_index)
            {
                *text = content.clone();
                *block_status = status;
                pushed_result = true;
                continue;
            }
        }
        blocks.push(ConversationBlockDto::ToolCall {
            id: tool_call_id.clone(),
            name: fallback_name.clone(),
            arguments: String::new(),
            text: content.clone(),
            status,
            metadata: None,
            arguments_json: None,
        });
        pushed_result = true;
    }

    if !pushed_result {
        blocks.push(ConversationBlockDto::ToolCall {
            id: fallback_id,
            name: fallback_name,
            arguments: String::new(),
            text: visible_message_text(message),
            status: ConversationBlockStatusDto::Complete,
            metadata: None,
            arguments_json: None,
        });
    }
}

fn visible_message_text(message: &LlmMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            LlmContent::ToolCall { .. } | LlmContent::Image { .. } => None,
            other => Some(other.to_display_text()),
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmMessage, TURN_ABORTED_SOURCE, turn_aborted_context_message},
        storage::SequencedLlmMessage,
    };

    use super::*;

    #[test]
    fn messages_to_blocks_hides_turn_aborted_context() {
        let messages = vec![
            SequencedLlmMessage {
                message: LlmMessage::user("visible"),
                updated_seq: 1,
                source: None,
            },
            SequencedLlmMessage {
                message: turn_aborted_context_message(),
                updated_seq: 2,
                source: Some(TURN_ABORTED_SOURCE.into()),
            },
        ];

        let blocks = messages_to_blocks(&messages);

        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ConversationBlockDto::User { text, .. } if text == "visible"
        ));
    }
}
