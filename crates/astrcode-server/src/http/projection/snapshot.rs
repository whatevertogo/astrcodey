//! Session read model -> conversation snapshot DTO projection.

use astrcode_core::{
    storage::{PendingToolApprovalView, PendingToolInteractionView, SessionReadModel},
    types::ToolCallId,
};
use astrcode_protocol::http::{
    AgentSessionLinkDto, ConversationBlockDto, ConversationBlockStatusDto, ConversationCursorDto,
    ConversationSnapshotResponseDto,
};

use super::{
    blocks::{compact_summary_block, latest_compact_boundary, messages_to_blocks},
    live::control_from_phase,
};
use crate::server_event_bus::StreamingSnapshot;

pub(in crate::http) fn conversation_to_dto(
    session: SessionReadModel,
    streaming: Option<&StreamingSnapshot>,
) -> ConversationSnapshotResponseDto {
    let title = session
        .first_user_message()
        .unwrap_or_else(|| session_title(&session.working_dir));

    // 与 provider_messages 一致：最新 compact 摘要紧挨保留消息之前（被压掉的历史不在 UI 展示）
    let mut blocks: Vec<ConversationBlockDto> = Vec::new();
    if let Some(boundary) = latest_compact_boundary(&session.compact_boundaries) {
        blocks.push(compact_summary_block(boundary));
    }
    blocks.extend(messages_to_blocks(&session.messages));
    apply_pending_tool_approvals(&mut blocks, &session.pending_tool_approvals);
    apply_pending_tool_interactions(&mut blocks, &session.pending_tool_interactions);

    // 如果有正在流式传输的 assistant 消息，追加一个 streaming block。
    // durable 投影不含 streaming 消息（`AssistantTextDelta` 是 live 事件），
    // 需要从 runtime 的 live 投影补充，让重连客户端看到已流出的文本。
    if let Some(msg) = streaming {
        blocks.push(ConversationBlockDto::Assistant {
            id: msg.message_id.clone(),
            text: msg.text.clone(),
            reasoning_content: msg.reasoning_content.clone(),
            status: ConversationBlockStatusDto::Streaming,
        });
    }

    ConversationSnapshotResponseDto {
        session_id: session.session_id.to_string(),
        session_title: title,
        cursor: ConversationCursorDto {
            value: session.cursor(),
        },
        phase: session.phase,
        control: control_from_phase(session.phase, !session.messages.is_empty()),
        blocks,
        agent_sessions: session
            .agent_sessions
            .iter()
            .map(AgentSessionLinkDto::from_view)
            .collect(),
    }
}

fn apply_pending_tool_interactions(
    blocks: &mut [ConversationBlockDto],
    pending: &std::collections::BTreeMap<ToolCallId, PendingToolInteractionView>,
) {
    for block in blocks.iter_mut() {
        let ConversationBlockDto::ToolCall {
            id,
            text,
            metadata,
            status,
            ..
        } = block
        else {
            continue;
        };
        let Some(interaction) = pending.get(&ToolCallId::from(id.as_str())) else {
            continue;
        };
        *text = interaction.content.clone();
        *status = ConversationBlockStatusDto::Streaming;
        let mut merged = metadata
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let interaction_metadata = serde_json::to_value(&interaction.metadata)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        if let Some(interaction_metadata) = interaction_metadata.as_object() {
            merged.extend(interaction_metadata.clone());
        }
        *metadata = if merged.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(merged))
        };
    }
}

fn apply_pending_tool_approvals(
    blocks: &mut [ConversationBlockDto],
    pending: &std::collections::BTreeMap<ToolCallId, PendingToolApprovalView>,
) {
    for block in blocks.iter_mut() {
        let ConversationBlockDto::ToolCall { id, metadata, .. } = block else {
            continue;
        };
        let Some(approval) = pending.get(&ToolCallId::from(id.as_str())) else {
            continue;
        };

        let mut merged = metadata
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        merged.insert(
            "toolGateApproval".into(),
            serde_json::json!({
                "pending": true,
                "prompt": &approval.prompt,
                "ruleKey": &approval.rule_key,
            }),
        );
        *metadata = Some(serde_json::Value::Object(merged));
    }
}

fn session_title(working_dir: &str) -> String {
    std::path::Path::new(working_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(working_dir)
        .to_string()
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};
    use astrcode_protocol::http::ConversationBlockStatusDto;

    use super::*;

    #[test]
    fn conversation_snapshot_cursor_is_full_snapshot_version() {
        let mut session = SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(9);
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage::user("hello"),
                updated_seq: 1,
                source: None,
            });

        let dto = conversation_to_dto(session, None);

        assert_eq!(dto.cursor.value, "9");
        assert_eq!(dto.blocks.len(), 1);
    }

    #[test]
    fn conversation_snapshot_renders_tool_call_as_structured_block() {
        let mut session = SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: "tool-1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "Cargo.toml" }),
                    }],
                    name: None,
                    reasoning_content: None,
                },
                updated_seq: 1,
                source: None,
            });
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage::tool("read", "tool-1", "file contents", false),
                updated_seq: 2,
                source: None,
            });

        let dto = conversation_to_dto(session, None);

        assert_eq!(dto.blocks.len(), 1);
        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                id,
                name,
                arguments,
                text,
                status,
                ..
            } => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "read");
                assert_eq!(arguments, "Cargo.toml");
                assert_eq!(text, "file contents");
                assert!(matches!(status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn conversation_snapshot_applies_pending_tool_approval_metadata() {
        let mut session = SessionReadModel::empty("session-approval".into());
        session.working_dir = "D:/work/project".into();
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: "tool-approval".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({ "command": "git push" }),
                    }],
                    name: None,
                    reasoning_content: None,
                },
                updated_seq: 1,
                source: None,
            });
        session.pending_tool_approvals.insert(
            "tool-approval".into(),
            astrcode_core::storage::PendingToolApprovalView {
                prompt: "Run shell command?".into(),
                rule_key: Some("shell:write".into()),
            },
        );

        let dto = conversation_to_dto(session, None);

        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                metadata: Some(metadata),
                ..
            } => {
                let approval = metadata
                    .get("toolGateApproval")
                    .expect("toolGateApproval metadata");
                assert_eq!(
                    approval.get("pending").and_then(|v| v.as_bool()),
                    Some(true)
                );
                assert_eq!(
                    approval.get("prompt").and_then(|v| v.as_str()),
                    Some("Run shell command?")
                );
                assert_eq!(
                    approval.get("ruleKey").and_then(|v| v.as_str()),
                    Some("shell:write")
                );
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn conversation_snapshot_places_compact_summary_before_retained_messages() {
        use astrcode_core::{extension::CompactStrategy, storage::CompactBoundaryView};

        let mut session = SessionReadModel::empty("session-compact".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(7);
        // compact 之后的 retained messages
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage::user("recent user"),
                updated_seq: 1,
                source: None,
            });
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage::assistant("recent assistant"),
                updated_seq: 2,
                source: None,
            });
        // compact boundary 元数据
        session.compact_boundaries.push(CompactBoundaryView {
            trigger: "manual_command".into(),
            pre_tokens: 1000,
            post_tokens: 200,
            summary: "Earlier conversation was compacted".into(),
            transcript_path: None,
            seq: 5,
            base_event_seq: 4,
            strategy: CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        });

        let dto = conversation_to_dto(session, None);

        // 顺序：CompactSummary → User → Assistant
        assert_eq!(dto.blocks.len(), 3);
        assert!(matches!(
            &dto.blocks[0],
            ConversationBlockDto::CompactSummary { .. }
        ));
        assert!(matches!(&dto.blocks[1], ConversationBlockDto::User { .. }));
        assert!(matches!(
            &dto.blocks[2],
            ConversationBlockDto::Assistant { .. }
        ));
    }

    #[test]
    fn conversation_snapshot_shows_only_latest_compact_before_retained_messages() {
        use astrcode_core::{extension::CompactStrategy, storage::CompactBoundaryView};

        use crate::http::projection::blocks::COMPACT_SUMMARY_BLOCK_ID;

        let mut session = SessionReadModel::empty("session-multi-compact".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(20);
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage::user("latest user"),
                updated_seq: 1,
                source: None,
            });
        session.compact_boundaries.push(CompactBoundaryView {
            trigger: "auto_threshold".into(),
            pre_tokens: 800,
            post_tokens: 100,
            summary: "First compaction".into(),
            transcript_path: None,
            seq: 5,
            base_event_seq: 4,
            strategy: CompactStrategy::Auto,
        });
        session.compact_boundaries.push(CompactBoundaryView {
            trigger: "auto_threshold".into(),
            pre_tokens: 600,
            post_tokens: 80,
            summary: "Second compaction".into(),
            transcript_path: None,
            seq: 12,
            base_event_seq: 11,
            strategy: CompactStrategy::Auto,
        });

        let dto = conversation_to_dto(session, None);

        assert_eq!(dto.blocks.len(), 2);
        match &dto.blocks[0] {
            ConversationBlockDto::CompactSummary { id, summary, .. } => {
                assert_eq!(id, COMPACT_SUMMARY_BLOCK_ID);
                assert_eq!(summary, "Second compaction");
            },
            other => panic!("expected CompactSummary, got {other:?}"),
        }
        assert!(matches!(&dto.blocks[1], ConversationBlockDto::User { .. }));
    }
}
