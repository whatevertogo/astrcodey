//! Session read model -> conversation snapshot DTO projection.

use std::collections::BTreeMap;

use astrcode_core::{
    storage::{PendingToolApprovalView, PendingToolInteractionView, SessionReadModel},
    types::ToolCallId,
};
use astrcode_protocol::http::{
    AgentSessionLinkDto, ConversationBlockDto, ConversationBlockStatusDto, ConversationCursorDto,
    ConversationSnapshotResponseDto,
};

use super::{
    blocks::{
        compact_summary_block, latest_compact_boundary, messages_to_blocks,
        streaming_assistant_block,
    },
    live::control_from_phase,
    session_title_from_working_dir,
};
use crate::server_event_bus::StreamingSnapshot;

pub(in crate::http) fn conversation_to_dto(
    session: SessionReadModel,
    streaming: Option<&StreamingSnapshot>,
) -> ConversationSnapshotResponseDto {
    let title = session
        .first_user_message()
        .unwrap_or_else(|| session_title_from_working_dir(&session.working_dir));

    // 与 provider_messages 一致：最新 compact 摘要紧挨保留消息之前（被压掉的历史不在 UI 展示）
    let mut blocks: Vec<ConversationBlockDto> = Vec::new();
    if let Some(boundary) = latest_compact_boundary(&session.compact_boundaries) {
        blocks.push(compact_summary_block(boundary));
    }
    blocks.extend(messages_to_blocks(&session.messages));
    apply_pending_tool_state(
        &mut blocks,
        &session.pending_tool_approvals,
        &session.pending_tool_interactions,
    );

    // 如果有正在流式传输的 assistant 消息，追加一个 streaming block。
    // durable 投影不含 streaming 消息（`AssistantTextDelta` 是 live 事件），
    // 需要从 runtime 的 live 投影补充，让重连客户端看到已流出的文本。
    if let Some(msg) = streaming {
        blocks.push(streaming_assistant_block(
            msg.message_id.clone(),
            msg.text.clone(),
            msg.reasoning_content.clone(),
        ));
    }

    ConversationSnapshotResponseDto {
        session_id: session.session_id.to_string(),
        session_title: title,
        cursor: ConversationCursorDto {
            value: session.cursor(),
        },
        phase: session.phase.into(),
        control: control_from_phase(session.phase, !session.messages.is_empty()),
        blocks,
        agent_sessions: session
            .agent_sessions
            .iter()
            .map(AgentSessionLinkDto::from_view)
            .collect(),
    }
}

fn apply_pending_tool_state(
    blocks: &mut [ConversationBlockDto],
    approvals: &BTreeMap<ToolCallId, PendingToolApprovalView>,
    interactions: &BTreeMap<ToolCallId, PendingToolInteractionView>,
) {
    for block in blocks {
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
        let approval = approvals.get(id.as_str());
        let interaction = interactions.get(id.as_str());
        if approval.is_none() && interaction.is_none() {
            continue;
        }

        let mut merged = match metadata.take() {
            Some(serde_json::Value::Object(metadata)) => metadata,
            _ => serde_json::Map::new(),
        };
        if let Some(approval) = approval {
            merged.insert(
                "toolGateApproval".into(),
                serde_json::json!({
                    "pending": true,
                    "prompt": &approval.prompt,
                    "ruleKey": &approval.rule_key,
                }),
            );
        }
        if let Some(interaction) = interaction {
            *text = interaction.content.clone();
            *status = ConversationBlockStatusDto::Streaming;
            merged.extend(
                interaction
                    .metadata
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        *metadata = (!merged.is_empty()).then_some(serde_json::Value::Object(merged));
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};
    use astrcode_protocol::http::ConversationBlockStatusDto;

    use super::*;

    fn session_with_tool_call(
        session_id: &str,
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> SessionReadModel {
        let mut session = SessionReadModel::empty(session_id.into());
        session.working_dir = "D:/work/project".into();
        session
            .messages
            .push(astrcode_core::storage::SequencedLlmMessage {
                message: LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: call_id.into(),
                        name: name.into(),
                        arguments,
                    }],
                    name: None,
                    reasoning_content: None,
                },
                updated_seq: 1,
                source: None,
            });
        session
    }

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
        let mut session = session_with_tool_call(
            "session-1",
            "tool-1",
            "read",
            serde_json::json!({ "path": "Cargo.toml" }),
        );
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
    fn conversation_snapshot_applies_pending_tool_state() {
        let mut session = session_with_tool_call(
            "session-approval",
            "tool-approval",
            "shell",
            serde_json::json!({ "command": "git push" }),
        );
        session.pending_tool_approvals.insert(
            "tool-approval".into(),
            astrcode_core::storage::PendingToolApprovalView {
                prompt: "Run shell command?".into(),
                rule_key: Some("shell:write".into()),
            },
        );
        session.pending_tool_interactions.insert(
            "tool-approval".into(),
            PendingToolInteractionView {
                content: "awaiting confirmation".into(),
                metadata: BTreeMap::from([(
                    "toolUi".into(),
                    serde_json::json!({ "kind": "confirmation" }),
                )]),
            },
        );

        let dto = conversation_to_dto(session, None);

        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                text,
                status,
                metadata: Some(metadata),
                ..
            } => {
                assert_eq!(text, "awaiting confirmation");
                assert!(matches!(status, ConversationBlockStatusDto::Streaming));
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
                assert_eq!(metadata["toolUi"]["kind"], "confirmation");
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
