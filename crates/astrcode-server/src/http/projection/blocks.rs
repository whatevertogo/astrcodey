//! 把 LLM message 历史与 EventPayload 投影成 ConversationBlockDto。

use std::collections::BTreeMap;

use astrcode_context::is_synthetic_context_message;
use astrcode_core::{
    event::{DurableEventPayload, Event, EventPayload, LiveEventPayload, TranscriptRewriteReason},
    llm::{LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, attachments_from_user_message},
    types::ToolCallId,
};
use astrcode_protocol::http::{
    ConversationBlockDto, ConversationBlockStatusDto, ToolCallStatusDto,
};
use astrcode_session_projection::{
    CompactionView, SequencedLlmMessage, TOOL_CALL_CANCELLED_SOURCE, TOOL_CALL_FAILED_SOURCE,
    TranscriptArtifactView,
};

use super::{args::format_args_inline, non_empty_metadata};

/// 对话 UI 中 compact 卡片的稳定 block id（多次 compact 时 upsert / 刷新替换）。
pub(in crate::http) const COMPACT_SUMMARY_BLOCK_ID: &str = "compact-current";

/// 仅用于对话展示：返回最近一次 compact 记录。
pub(in crate::http) fn latest_compaction(
    compactions: &[CompactionView],
) -> Option<&CompactionView> {
    compactions.iter().max_by_key(|compaction| compaction.seq)
}

/// 将 compact 记录投影为对话 block（插在保留消息之前）。
pub(in crate::http) fn compact_summary_block(compaction: &CompactionView) -> ConversationBlockDto {
    ConversationBlockDto::CompactSummary {
        id: COMPACT_SUMMARY_BLOCK_ID.to_string(),
        summary: compaction.summary.clone(),
        trigger: compaction.trigger.clone(),
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        transcript_path: compaction.transcript_path.clone(),
    }
}

/// 构建 live、replay 和 snapshot 共用的流式助手 block。
pub(in crate::http) fn streaming_assistant_block(
    id: String,
    text: String,
    reasoning_content: Option<String>,
) -> ConversationBlockDto {
    ConversationBlockDto::Assistant {
        id,
        text,
        reasoning_content,
        status: ConversationBlockStatusDto::Streaming,
    }
}

/// 构建 live、replay 和 snapshot 共用的流式工具调用 block。
pub(in crate::http) fn streaming_tool_call_block(
    id: String,
    name: &str,
    arguments: Option<&serde_json::Value>,
) -> ConversationBlockDto {
    ConversationBlockDto::ToolCall {
        id,
        name: name.to_owned(),
        arguments: arguments.map_or_else(String::new, |value| format_args_inline(name, value)),
        text: String::new(),
        status: ToolCallStatusDto::Streaming,
        metadata: None,
        approval: None,
        arguments_json: arguments.cloned(),
    }
}

/// 为单个可见 block 的 payload 构建共享投影。
pub(in crate::http) fn block_from_payload(event: &Event) -> Option<ConversationBlockDto> {
    let payload = match &event.payload {
        EventPayload::Durable(payload) => payload,
        EventPayload::Live(LiveEventPayload::ErrorOccurred { message, .. }) => {
            return Some(ConversationBlockDto::Error {
                id: event.id.to_string(),
                message: message.clone(),
            });
        },
        EventPayload::Live(_) => return None,
    };
    match payload {
        DurableEventPayload::UserMessage {
            message_id,
            text,
            attachments,
            ..
        } => Some(ConversationBlockDto::User {
            id: message_id.to_string(),
            text: text.clone(),
            attachments: attachments.iter().cloned().map(Into::into).collect(),
            source: None,
        }),
        DurableEventPayload::AssistantMessageCompleted {
            message_id,
            text,
            reasoning_content,
        } => Some(ConversationBlockDto::Assistant {
            id: message_id.to_string(),
            text: text.clone(),
            reasoning_content: reasoning_content.clone(),
            status: ConversationBlockStatusDto::Complete,
        }),
        DurableEventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            arguments,
            arguments_json,
            ..
        } => Some(tool_call_terminal_block(
            call_id,
            tool_name,
            arguments,
            arguments_json.as_ref(),
            result.content.clone(),
            ToolCallStatusDto::Complete,
            tool_terminal_metadata(&result.metadata, result.duration_ms),
        )),
        DurableEventPayload::ToolCallFailed {
            call_id,
            tool_name,
            error,
            metadata,
            duration_ms,
            arguments,
            arguments_json,
        } => Some(tool_call_terminal_block(
            call_id,
            tool_name,
            arguments,
            arguments_json.as_ref(),
            error.clone(),
            ToolCallStatusDto::Failed,
            tool_terminal_metadata(metadata, *duration_ms),
        )),
        DurableEventPayload::ToolCallCancelled {
            call_id,
            tool_name,
            reason,
            duration_ms,
            arguments,
            arguments_json,
        } => Some(tool_call_terminal_block(
            call_id,
            tool_name,
            arguments,
            arguments_json.as_ref(),
            format!("Tool cancelled: {reason}"),
            ToolCallStatusDto::Cancelled,
            tool_terminal_metadata(&BTreeMap::new(), *duration_ms),
        )),
        DurableEventPayload::ErrorOccurred { message, .. } => Some(ConversationBlockDto::Error {
            id: event.id.to_string(),
            message: message.clone(),
        }),
        DurableEventPayload::TranscriptRewritten {
            reason: TranscriptRewriteReason::Compaction(details),
            ..
        } => Some(ConversationBlockDto::CompactSummary {
            id: COMPACT_SUMMARY_BLOCK_ID.to_string(),
            summary: details.summary.clone(),
            trigger: details.trigger.clone(),
            pre_tokens: details.pre_tokens,
            post_tokens: details.post_tokens,
            transcript_path: details.transcript_path.clone(),
        }),
        DurableEventPayload::RecapGenerated { text, source } => Some(ConversationBlockDto::Recap {
            id: event.id.to_string(),
            text: text.clone(),
            source: source.clone(),
        }),
        _ => None,
    }
}

/// 构造工具终态（completed/failed/cancelled）的 ToolCall block。
fn tool_call_terminal_block(
    call_id: &ToolCallId,
    tool_name: &str,
    arguments: &str,
    arguments_json: Option<&serde_json::Value>,
    text: String,
    status: ToolCallStatusDto,
    metadata: Option<serde_json::Value>,
) -> ConversationBlockDto {
    ConversationBlockDto::ToolCall {
        id: call_id.to_string(),
        name: tool_name.to_owned(),
        arguments: arguments.to_owned(),
        text,
        status,
        metadata,
        approval: None,
        arguments_json: arguments_json.cloned(),
    }
}

pub(in crate::http) fn transcript_blocks(
    messages: &[SequencedLlmMessage],
    artifacts: &[TranscriptArtifactView],
) -> Vec<ConversationBlockDto> {
    let mut blocks = sequenced_message_blocks(messages);
    blocks.extend(artifacts.iter().map(|artifact| SequencedConversationBlock {
        seq: artifact.seq(),
        block: transcript_artifact_block(artifact),
    }));
    blocks.sort_by_key(|entry| entry.seq);
    blocks.into_iter().map(|entry| entry.block).collect()
}

struct SequencedConversationBlock {
    seq: u64,
    block: ConversationBlockDto,
}

fn sequenced_message_blocks(messages: &[SequencedLlmMessage]) -> Vec<SequencedConversationBlock> {
    let mut blocks = Vec::new();
    let mut tool_block_indices = BTreeMap::new();

    for (index, seq_msg) in messages.iter().enumerate() {
        let message = &seq_msg.message;
        let source = &seq_msg.source;
        if source.as_deref() == Some(TURN_ABORTED_SOURCE) || is_synthetic_context_message(message) {
            continue;
        }
        let id = format!("snapshot-message-{index}");
        match message.role {
            LlmRole::User => blocks.push(SequencedConversationBlock {
                seq: seq_msg.updated_seq,
                block: ConversationBlockDto::User {
                    id,
                    text: visible_ui_text(message),
                    attachments: attachments_from_user_message(message)
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    source: source.clone(),
                },
            }),
            LlmRole::Assistant => {
                let text = visible_ui_text(message);
                if !text.trim().is_empty() || message.reasoning_content.is_some() {
                    blocks.push(SequencedConversationBlock {
                        seq: seq_msg.updated_seq,
                        block: ConversationBlockDto::Assistant {
                            id,
                            text,
                            reasoning_content: message.reasoning_content.clone(),
                            status: ConversationBlockStatusDto::Complete,
                        },
                    });
                }
                for content in &message.content {
                    let LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                        raw_arguments,
                    } = content
                    else {
                        continue;
                    };
                    let block_index = blocks.len();
                    blocks.push(SequencedConversationBlock {
                        seq: seq_msg.updated_seq,
                        block: streaming_tool_call_block(
                            call_id.clone(),
                            name,
                            raw_arguments.is_none().then_some(arguments),
                        ),
                    });
                    tool_block_indices.insert(call_id.clone(), block_index);
                }
            },
            LlmRole::Tool => {
                push_tool_result_block(
                    &mut blocks,
                    &tool_block_indices,
                    message,
                    id,
                    seq_msg.updated_seq,
                    source.as_deref(),
                );
            },
            LlmRole::System => blocks.push(SequencedConversationBlock {
                seq: seq_msg.updated_seq,
                block: ConversationBlockDto::SystemNote {
                    id,
                    text: visible_ui_text(message),
                },
            }),
        }
    }

    blocks
}

fn push_tool_result_block(
    blocks: &mut Vec<SequencedConversationBlock>,
    tool_block_indices: &BTreeMap<String, usize>,
    message: &LlmMessage,
    fallback_id: String,
    seq: u64,
    source: Option<&str>,
) {
    let fallback_name = message.name.clone().unwrap_or_else(|| "tool".into());
    let mut pushed_result = false;

    for content in &message.content {
        let LlmContent::ToolResult {
            tool_call_id,
            content,
            ..
        } = content
        else {
            continue;
        };
        let status = tool_status_from_message(source);
        if let Some(block_index) = tool_block_indices.get(tool_call_id) {
            if let Some(SequencedConversationBlock {
                block:
                    ConversationBlockDto::ToolCall {
                        text,
                        status: block_status,
                        ..
                    },
                ..
            }) = blocks.get_mut(*block_index)
            {
                text.clone_from(content);
                *block_status = status;
                pushed_result = true;
                continue;
            }
        }
        blocks.push(SequencedConversationBlock {
            seq,
            block: ConversationBlockDto::ToolCall {
                id: tool_call_id.clone(),
                name: fallback_name.clone(),
                arguments: String::new(),
                text: content.clone(),
                status,
                metadata: None,
                approval: None,
                arguments_json: None,
            },
        });
        pushed_result = true;
    }

    if !pushed_result {
        blocks.push(SequencedConversationBlock {
            seq,
            block: ConversationBlockDto::ToolCall {
                id: fallback_id,
                name: fallback_name,
                arguments: String::new(),
                text: visible_ui_text(message),
                status: tool_status_from_message(source),
                metadata: None,
                approval: None,
                arguments_json: None,
            },
        });
    }
}

fn tool_status_from_message(source: Option<&str>) -> ToolCallStatusDto {
    match source {
        Some(TOOL_CALL_FAILED_SOURCE) => ToolCallStatusDto::Failed,
        Some(TOOL_CALL_CANCELLED_SOURCE) => ToolCallStatusDto::Cancelled,
        // `ToolResult.is_error` describes result semantics, not tool-call lifecycle.
        _ => ToolCallStatusDto::Complete,
    }
}

fn tool_terminal_metadata(
    metadata: &BTreeMap<String, serde_json::Value>,
    duration_ms: Option<u64>,
) -> Option<serde_json::Value> {
    let mut metadata = metadata.clone();
    if let Some(duration_ms) = duration_ms {
        metadata.insert("durationMs".into(), duration_ms.into());
    }
    non_empty_metadata(&metadata)
}

fn transcript_artifact_block(artifact: &TranscriptArtifactView) -> ConversationBlockDto {
    match artifact {
        TranscriptArtifactView::Error { id, message, .. } => ConversationBlockDto::Error {
            id: id.clone(),
            message: message.clone(),
        },
        TranscriptArtifactView::SystemNote { id, text, .. } => ConversationBlockDto::SystemNote {
            id: id.clone(),
            text: text.clone(),
        },
    }
}

/// 提取 UI 可展示的文本：跳过 ToolCall/Image（与 compaction 用的
/// `visible_message_text` 不同——后者会把工具调用渲染成文本给摘要模型读）。
fn visible_ui_text(message: &LlmMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            LlmContent::ToolCall { .. } | LlmContent::Image { .. } => None,
            other => Some(other.to_display_text()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{
        LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, turn_aborted_context_message,
    };
    use astrcode_session_projection::{
        SequencedLlmMessage, TOOL_CALL_CANCELLED_SOURCE, TOOL_CALL_FAILED_SOURCE,
    };

    use super::*;

    #[test]
    fn transcript_blocks_hides_turn_aborted_context() {
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

        let blocks = transcript_blocks(&messages, &[]);

        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ConversationBlockDto::User { text, .. } if text == "visible"
        ));
    }

    #[test]
    fn transcript_blocks_restore_tool_terminal_statuses() {
        let cases = [
            ("complete", None, false, ToolCallStatusDto::Complete),
            (
                "completed-error-result",
                None,
                true,
                ToolCallStatusDto::Complete,
            ),
            (
                "failed",
                Some(TOOL_CALL_FAILED_SOURCE),
                true,
                ToolCallStatusDto::Failed,
            ),
            (
                "cancelled",
                Some(TOOL_CALL_CANCELLED_SOURCE),
                true,
                ToolCallStatusDto::Cancelled,
            ),
        ];
        let mut messages = vec![SequencedLlmMessage {
            message: LlmMessage {
                role: LlmRole::Assistant,
                content: cases
                    .iter()
                    .map(|(call_id, ..)| LlmContent::ToolCall {
                        call_id: (*call_id).into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({}),
                        raw_arguments: None,
                    })
                    .collect(),
                name: None,
                reasoning_content: None,
            },
            updated_seq: 1,
            source: None,
        }];
        messages.extend(
            cases
                .iter()
                .enumerate()
                .map(
                    |(index, (call_id, source, is_error, _))| SequencedLlmMessage {
                        message: LlmMessage::tool("probe", *call_id, *call_id, *is_error),
                        updated_seq: index as u64 + 2,
                        source: source.map(str::to_owned),
                    },
                ),
        );

        let blocks = transcript_blocks(&messages, &[]);
        let statuses = blocks
            .iter()
            .map(|block| {
                let ConversationBlockDto::ToolCall { status, .. } = block else {
                    panic!("expected tool block");
                };
                *status
            })
            .collect::<Vec<_>>();

        assert_eq!(
            statuses,
            cases
                .into_iter()
                .map(|(_, _, _, status)| status)
                .collect::<Vec<_>>()
        );
    }
}
