//! Session read model -> conversation snapshot DTO projection.

use std::collections::BTreeMap;

use astrcode_core::types::ToolCallId;
use astrcode_protocol::http::{
    ConversationBlockDto, ConversationCursorDto, ConversationSnapshotResponseDto, ToolApprovalDto,
};
use astrcode_session_projection::{PendingToolApprovalView, SessionReadModel};

use super::{
    blocks::{
        compact_summary_block, latest_compaction, streaming_assistant_block, transcript_blocks,
    },
    live::control_from_phase,
    session_title_from_working_dir,
};
use crate::{protocol_mapping::agent_session_link_to_dto, server_event_bus::StreamingSnapshot};

pub(in crate::http) fn conversation_to_dto(
    session: &SessionReadModel,
    streaming: Option<&StreamingSnapshot>,
) -> ConversationSnapshotResponseDto {
    let title = session
        .first_user_message()
        .map(str::to_owned)
        .unwrap_or_else(|| session_title_from_working_dir(&session.identity.working_dir));

    // 与 provider_messages 一致：最新 compact 摘要紧挨保留消息之前（被压掉的历史不在 UI 展示）
    let mut blocks: Vec<ConversationBlockDto> = Vec::new();
    if let Some(compaction) = latest_compaction(&session.compactions) {
        blocks.push(compact_summary_block(compaction));
    }
    blocks.extend(transcript_blocks(
        &session.transcript.messages,
        &session.transcript.artifacts,
    ));
    apply_pending_tool_approvals(&mut blocks, &session.execution.pending_tool_approvals);

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
        session_id: session.identity.session_id.to_string(),
        session_title: title,
        cursor: ConversationCursorDto {
            value: session.cursor(),
        },
        phase: session.execution.phase.into(),
        control: control_from_phase(
            session.execution.phase,
            !session.transcript.messages.is_empty(),
        ),
        blocks,
        agent_sessions: session
            .agent_sessions
            .iter()
            .map(agent_session_link_to_dto)
            .collect(),
    }
}

fn apply_pending_tool_approvals(
    blocks: &mut [ConversationBlockDto],
    approvals: &BTreeMap<ToolCallId, PendingToolApprovalView>,
) {
    for block in blocks {
        let ConversationBlockDto::ToolCall {
            id,
            approval: block_approval,
            ..
        } = block
        else {
            continue;
        };
        let Some(approval) = approvals.get(id.as_str()) else {
            continue;
        };
        *block_approval = Some(ToolApprovalDto {
            call_id: id.clone(),
            prompt: approval.prompt.clone(),
            rule_key: approval.rule_key.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{
            DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted, StoredEvent,
            SystemPromptSource,
        },
        llm::{LlmContent, LlmMessage, LlmRole},
        tool::SessionToolSelection,
    };
    use astrcode_protocol::http::ToolCallStatusDto;
    use astrcode_session_projection::replay;

    use super::*;

    fn session_read_model(session_id: &str) -> SessionReadModel {
        let session_id: astrcode_core::types::SessionId = session_id.into();
        replay(
            session_id.clone(),
            &[StoredEvent::new(
                0,
                DurableEvent::session(
                    session_id,
                    DurableEventPayload::SessionStarted(SessionStarted {
                        working_dir: "D:/work/project".into(),
                        model_id: "model".into(),
                        parent: None,
                        tool_selection: SessionToolSelection::default(),
                        source_extension: None,
                        initial_system_prompt: PersistedSystemPrompt {
                            text: "system".into(),
                            fingerprint: "fingerprint".into(),
                            extra_system_prompt: None,
                            source: SystemPromptSource::Native,
                        },
                    }),
                ),
            )],
        )
        .unwrap()
    }

    fn session_with_tool_call(
        session_id: &str,
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> SessionReadModel {
        let mut session = session_read_model(session_id);
        session
            .transcript
            .messages
            .push(astrcode_session_projection::SequencedLlmMessage {
                message: LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: call_id.into(),
                        name: name.into(),
                        arguments,
                        raw_arguments: None,
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
        let mut session = session_read_model("session-1");
        session.stats.last_seq = 9;
        session
            .transcript
            .messages
            .push(astrcode_session_projection::SequencedLlmMessage {
                message: LlmMessage::user("hello"),
                updated_seq: 1,
                source: None,
            });

        let dto = conversation_to_dto(&session, None);

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
            .transcript
            .messages
            .push(astrcode_session_projection::SequencedLlmMessage {
                message: LlmMessage::tool("read", "tool-1", "file contents", false),
                updated_seq: 2,
                source: None,
            });

        let dto = conversation_to_dto(&session, None);

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
                assert!(matches!(status, ToolCallStatusDto::Complete));
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn conversation_snapshot_applies_explicit_pending_tool_approval() {
        let mut session = session_with_tool_call(
            "session-approval",
            "tool-approval",
            "shell",
            serde_json::json!({ "command": "git push" }),
        );
        session.execution.pending_tool_approvals.insert(
            "tool-approval".into(),
            astrcode_session_projection::PendingToolApprovalView {
                prompt: "Run shell command?".into(),
                rule_key: Some("shell:write".into()),
            },
        );
        let dto = conversation_to_dto(&session, None);

        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                approval: Some(approval),
                ..
            } => {
                assert_eq!(approval.call_id, "tool-approval");
                assert_eq!(approval.prompt, "Run shell command?");
                assert_eq!(approval.rule_key.as_deref(), Some("shell:write"));
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn conversation_snapshot_shows_only_latest_compact_before_retained_messages() {
        use astrcode_core::compaction::CompactStrategy;
        use astrcode_session_projection::CompactionView;

        use crate::http::projection::blocks::COMPACT_SUMMARY_BLOCK_ID;

        let mut session = session_read_model("session-multi-compact");
        session.stats.last_seq = 20;
        session
            .transcript
            .messages
            .push(astrcode_session_projection::SequencedLlmMessage {
                message: LlmMessage::user("latest user"),
                updated_seq: 1,
                source: None,
            });
        session.compactions.push(CompactionView {
            trigger: "auto_threshold".into(),
            pre_tokens: 800,
            post_tokens: 100,
            summary: "First compaction".into(),
            transcript_path: None,
            seq: 5,
            source_seq: 4,
            strategy: CompactStrategy::Auto,
        });
        session.compactions.push(CompactionView {
            trigger: "auto_threshold".into(),
            pre_tokens: 600,
            post_tokens: 80,
            summary: "Second compaction".into(),
            transcript_path: None,
            seq: 12,
            source_seq: 11,
            strategy: CompactStrategy::Auto,
        });

        let dto = conversation_to_dto(&session, None);

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
