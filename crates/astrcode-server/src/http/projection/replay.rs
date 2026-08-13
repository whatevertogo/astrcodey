//! 重放历史事件 → ConversationDeltaDto。

use astrcode_core::event::{DurableEventPayload, Event, Phase};
use astrcode_protocol::http::{ConversationDeltaDto, ToolApprovalDto};

use super::{
    blocks::{block_from_payload, streaming_tool_call_block},
    live::{control_from_phase, custom_event_delta},
};

pub(in crate::http) fn event_to_replay_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    let Some(payload) = event.payload.as_durable() else {
        return Vec::new();
    };
    if matches!(
        payload,
        DurableEventPayload::TranscriptRewritten { .. } | DurableEventPayload::SessionForked { .. }
    ) {
        return vec![ConversationDeltaDto::RehydrateRequired];
    }

    if let Some(block) = block_from_payload(event) {
        return vec![ConversationDeltaDto::AppendBlock { block }];
    }
    if let DurableEventPayload::ToolCallRequested {
        call_id,
        tool_name,
        arguments,
        raw_arguments,
    } = payload
    {
        return vec![ConversationDeltaDto::AppendBlock {
            block: streaming_tool_call_block(
                call_id.to_string(),
                tool_name,
                raw_arguments.is_none().then_some(arguments),
            ),
        }];
    }
    match payload {
        DurableEventPayload::TurnStarted => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(Phase::Thinking, has_messages),
            }]
        },
        DurableEventPayload::TurnCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(Phase::Idle, has_messages),
            }]
        },
        DurableEventPayload::ToolApprovalRequested {
            call_id,
            prompt,
            rule_key,
            ..
        } => vec![ConversationDeltaDto::ToolApprovalRequested {
            approval: ToolApprovalDto {
                call_id: call_id.to_string(),
                prompt: prompt.clone(),
                rule_key: rule_key.clone(),
            },
        }],
        DurableEventPayload::ToolApprovalResolved {
            call_id, decision, ..
        } => vec![ConversationDeltaDto::ToolApprovalResolved {
            call_id: call_id.to_string(),
            decision: (*decision).into(),
        }],
        DurableEventPayload::CustomEvent(extension) => {
            vec![custom_event_delta(extension)]
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        compaction::CompactStrategy,
        event::{
            CompactionDetails, DurableEvent, DurableEventPayload, StoredEvent,
            TranscriptRewriteReason,
        },
        types::{SessionId, ToolCallId, new_message_id},
    };

    use super::*;

    #[test]
    fn compact_replay_preserves_rehydrate_signal() {
        let rewrite = Event::from(StoredEvent::new(
            7,
            DurableEvent::session(
                "session-1".into(),
                DurableEventPayload::TranscriptRewritten {
                    source_seq: 0,
                    source_fingerprint: "fingerprint".into(),
                    messages: Vec::new(),
                    reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                        trigger: "manual_command".into(),
                        pre_tokens: 100,
                        post_tokens: 20,
                        summary: "summary".into(),
                        transcript_path: Some("compact.jsonl".into()),
                        strategy: CompactStrategy::Manual {
                            keep_recent_turns: None,
                        },
                    }),
                },
            ),
        ));

        assert!(matches!(
            event_to_replay_deltas(&rewrite, true).as_slice(),
            [ConversationDeltaDto::RehydrateRequired]
        ));
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ReplayExpectation {
        AppendBlock,
        Rehydrate,
        ControlState,
        Approval,
        CustomEvent,
        Empty,
    }

    /// 每个 `DurableEventPayload` 变体在重放中应产出的 delta 类别。
    ///
    /// 此 match **故意不写通配臂**：新增 `DurableEventPayload` 变体会让本函数编译
    /// 失败，迫使作者显式归类其重放语义，从而堵住 `event_to_replay_deltas` 末尾
    /// `Vec::new()` 兜底把新变体静默丢弃的缺口。新增变体后，若其路由不属于现有
    /// 任意类别，请在 `replay_routes_each_payload_category` 补一个样本断言。
    fn replay_expectation(payload: &DurableEventPayload) -> ReplayExpectation {
        use DurableEventPayload::*;
        match payload {
            // 结构性改写，无法增量重放 → 客户端必须重拉快照。
            TranscriptRewritten { .. } | SessionForked { .. } => ReplayExpectation::Rehydrate,
            TurnStarted | TurnCompleted { .. } => ReplayExpectation::ControlState,
            ToolApprovalRequested { .. } | ToolApprovalResolved { .. } => {
                ReplayExpectation::Approval
            },
            CustomEvent(_) => ReplayExpectation::CustomEvent,
            // 经 block_from_payload 委托，或 ToolCallRequested 的流式 block 分支产出可见 block。
            UserMessage { .. }
            | AssistantMessageCompleted { .. }
            | ToolCallCompleted { .. }
            | ToolCallFailed { .. }
            | ToolCallCancelled { .. }
            | ErrorOccurred { .. }
            | RecapGenerated { .. }
            | ToolCallRequested { .. } => ReplayExpectation::AppendBlock,
            // 重放时不产出 delta 的载荷。SessionStarted 是首事件，cursor 之后不会出现。
            // 子 Agent 血缘 delta 仅在 live 流发出；重放依赖快照/rehydrate 重建。
            SessionStarted(_)
            | ModelIdChanged { .. }
            | SessionToolsConfigured { .. }
            | SystemPromptConfigured { .. }
            | StepStarted { .. }
            | StepCompleted { .. }
            | TurnAbortedContext
            | UserInputAccepted { .. }
            | TokenUsageRecorded { .. }
            | AgentSessionSpawned { .. }
            | AgentSessionCompleted { .. }
            | AgentSessionFailed { .. }
            | AgentSessionRecycled { .. } => ReplayExpectation::Empty,
        }
    }

    #[test]
    fn replay_routes_each_payload_category() {
        let session_id = SessionId::new("session-replay");
        let cases: Vec<DurableEventPayload> = vec![
            // 结构性改写 → RehydrateRequired
            DurableEventPayload::TranscriptRewritten {
                source_seq: 0,
                source_fingerprint: "fingerprint".into(),
                messages: Vec::new(),
                reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                    trigger: "manual_command".into(),
                    pre_tokens: 100,
                    post_tokens: 20,
                    summary: "summary".into(),
                    transcript_path: None,
                    strategy: CompactStrategy::Manual {
                        keep_recent_turns: None,
                    },
                }),
            },
            DurableEventPayload::SessionForked {
                source_session_id: session_id.clone(),
                source_cursor: "0".into(),
                first_user_message: None,
                messages: Vec::new(),
            },
            // 控制态切换
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
            DurableEventPayload::TurnStarted,
            DurableEventPayload::ToolApprovalRequested {
                call_id: ToolCallId::new("call-approval"),
                tool_name: "shell".into(),
                prompt: "Approve shell?".into(),
                rule_key: Some("shell:pwd".into()),
                source: astrcode_core::permission::ApprovalSource::Core,
                arguments: serde_json::json!({"cmd": "pwd"}),
            },
            DurableEventPayload::ToolApprovalResolved {
                call_id: ToolCallId::new("call-approval"),
                decision: astrcode_core::permission::ApprovalDecision::AllowOnce,
                detail: None,
            },
            DurableEventPayload::CustomEvent(astrcode_core::event::CustomEventData {
                extension_id: "extension".into(),
                event_type: "event".into(),
                schema_version: 1,
                causation_id: None,
                cascade_depth: 0,
                payload: serde_json::json!(["scalar-compatible"]),
            }),
            // 委托 block_from_payload → AppendBlock
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "hello".into(),
                attachments: Vec::new(),
                accepted_seq: None,
            },
            // 绕过 block_from_payload 的特殊 AppendBlock（流式工具调用 block）
            DurableEventPayload::ToolCallRequested {
                call_id: ToolCallId::new("call-1"),
                tool_name: "read".into(),
                arguments: serde_json::json!({"path": "README.md"}),
                raw_arguments: None,
            },
            // 无操作
            DurableEventPayload::ModelIdChanged {
                model_id: "model-b".into(),
            },
        ];

        for (seq, payload) in cases.into_iter().enumerate() {
            let expected = replay_expectation(&payload);
            let event = Event::from(StoredEvent::new(
                seq as u64,
                DurableEvent::session(session_id.clone(), payload),
            ));
            let deltas = event_to_replay_deltas(&event, true);
            assert_replay_delta(&deltas, expected, seq);
        }
    }

    fn assert_replay_delta(
        deltas: &[ConversationDeltaDto],
        expected: ReplayExpectation,
        seq: usize,
    ) {
        match expected {
            ReplayExpectation::AppendBlock => assert!(
                matches!(deltas, [ConversationDeltaDto::AppendBlock { .. }]),
                "seq {seq}: expected AppendBlock, got {deltas:?}"
            ),
            ReplayExpectation::Rehydrate => assert!(
                matches!(deltas, [ConversationDeltaDto::RehydrateRequired]),
                "seq {seq}: expected RehydrateRequired, got {deltas:?}"
            ),
            ReplayExpectation::ControlState => assert!(
                matches!(deltas, [ConversationDeltaDto::UpdateControlState { .. }]),
                "seq {seq}: expected UpdateControlState, got {deltas:?}"
            ),
            ReplayExpectation::Approval => assert!(
                matches!(
                    deltas,
                    [ConversationDeltaDto::ToolApprovalRequested { .. }]
                        | [ConversationDeltaDto::ToolApprovalResolved { .. }]
                ),
                "seq {seq}: expected approval delta, got {deltas:?}"
            ),
            ReplayExpectation::CustomEvent => assert!(
                matches!(deltas, [ConversationDeltaDto::CustomEvent { .. }]),
                "seq {seq}: expected CustomEvent, got {deltas:?}"
            ),
            ReplayExpectation::Empty => assert!(
                deltas.is_empty(),
                "seq {seq}: expected no delta, got {deltas:?}"
            ),
        }
    }
}
