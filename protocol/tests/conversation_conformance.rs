use astrcode_core::{CompactAppliedMeta, CompactMode, CompactTrigger};
use astrcode_protocol::http::{
    AgentLifecycleDto, ChildAgentRefDto, ChildSessionLineageKindDto,
    ConversationBannerErrorCodeDto, ConversationBlockDto, ConversationBlockPatchDto,
    ConversationBlockStatusDto, ConversationControlStateDto, ConversationCursorDto,
    ConversationDeltaDto, ConversationErrorEnvelopeDto, ConversationLastCompactMetaDto,
    ConversationPlanBlockDto, ConversationPlanBlockersDto, ConversationPlanEventKindDto,
    ConversationPlanReviewDto, ConversationPlanReviewKindDto, ConversationSnapshotResponseDto,
    ConversationStepProgressDto, ConversationStreamEnvelopeDto, ConversationSystemNoteBlockDto,
    ConversationSystemNoteKindDto, ConversationTaskItemDto, ConversationTaskStatusDto,
    ConversationToolCallBlockDto, ConversationToolStreamsDto, PhaseDto,
};
use serde_json::json;

fn fixture(name: &str) -> serde_json::Value {
    let path = match name {
        "snapshot" => include_str!("fixtures/conversation/v1/snapshot.json"),
        "delta_patch_tool_stream" => {
            include_str!("fixtures/conversation/v1/delta_patch_tool_stream.json")
        },
        "delta_rehydrate_required" => {
            include_str!("fixtures/conversation/v1/delta_rehydrate_required.json")
        },
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(path).expect("fixture should be valid JSON")
}

#[test]
fn conversation_snapshot_fixture_freezes_authoritative_tool_block_shape() {
    let expected = ConversationSnapshotResponseDto {
        session_id: "session-root".to_string(),
        session_title: "Conversation session".to_string(),
        cursor: ConversationCursorDto("cursor:opaque:v1:session-root/42==".to_string()),
        phase: PhaseDto::CallingTool,
        control: ConversationControlStateDto {
            phase: PhaseDto::CallingTool,
            can_submit_prompt: false,
            can_request_compact: true,
            compact_pending: false,
            compacting: false,
            current_mode_id: "code".to_string(),
            active_turn_id: Some("turn-42".to_string()),
            last_compact_meta: None,
            active_plan: None,
            active_tasks: Some(vec![
                ConversationTaskItemDto {
                    content: "实现 authoritative task panel".to_string(),
                    status: ConversationTaskStatusDto::InProgress,
                    active_form: Some("正在实现 authoritative task panel".to_string()),
                },
                ConversationTaskItemDto {
                    content: "补充前端 hydration 测试".to_string(),
                    status: ConversationTaskStatusDto::Pending,
                    active_form: None,
                },
            ]),
        },
        step_progress: ConversationStepProgressDto {
            durable: None,
            live: None,
        },
        blocks: vec![ConversationBlockDto::ToolCall(
            ConversationToolCallBlockDto {
                id: "block-tool-call-1".to_string(),
                turn_id: Some("turn-42".to_string()),
                tool_call_id: "tool-call-1".to_string(),
                tool_name: "spawn_agent".to_string(),
                status: ConversationBlockStatusDto::Failed,
                input: Some(json!({ "task": "inspect repo" })),
                summary: Some("permission denied".to_string()),
                error: Some("permission denied".to_string()),
                duration_ms: Some(88),
                truncated: true,
                metadata: Some(json!({
                    "display": {
                        "kind": "terminal",
                        "command": "python worker.py"
                    }
                })),
                child_ref: Some(ChildAgentRefDto {
                    agent_id: "agent-child-1".to_string(),
                    session_id: "session-root".to_string(),
                    sub_run_id: "subrun-child-1".to_string(),
                    parent_agent_id: Some("agent-root".to_string()),
                    parent_sub_run_id: Some("subrun-root".to_string()),
                    lineage_kind: ChildSessionLineageKindDto::Spawn,
                    status: AgentLifecycleDto::Running,
                    open_session_id: "session-child-1".to_string(),
                }),
                streams: ConversationToolStreamsDto {
                    stdout: "searching repo\n".to_string(),
                    stderr: "permission denied\n".to_string(),
                },
            },
        )],
        child_summaries: Vec::new(),
        slash_candidates: Vec::new(),
        banner: None,
    };

    let fixture = fixture("snapshot");
    let decoded: ConversationSnapshotResponseDto =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("snapshot should encode"),
        fixture
    );
}

#[test]
fn conversation_delta_fixtures_freeze_tool_patch_and_rehydrate_shapes() {
    let patch_fixture = fixture("delta_patch_tool_stream");
    let patch_decoded: ConversationStreamEnvelopeDto =
        serde_json::from_value(patch_fixture.clone()).expect("patch fixture should decode");
    assert_eq!(
        patch_decoded,
        ConversationStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: ConversationCursorDto("cursor:opaque:v1:session-root/44==".to_string()),
            step_progress: ConversationStepProgressDto {
                durable: None,
                live: None,
            },
            delta: ConversationDeltaDto::PatchBlock {
                block_id: "block-tool-call-1".to_string(),
                patch: ConversationBlockPatchDto::AppendToolStream {
                    stream: astrcode_protocol::http::ToolOutputStreamDto::Stderr,
                    chunk: "line 1\nline 2".to_string(),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&patch_decoded).expect("patch should encode"),
        patch_fixture
    );

    let rehydrate_fixture = fixture("delta_rehydrate_required");
    let rehydrate_decoded: ConversationStreamEnvelopeDto =
        serde_json::from_value(rehydrate_fixture.clone()).expect("rehydrate fixture should decode");
    assert_eq!(
        rehydrate_decoded,
        ConversationStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: ConversationCursorDto("cursor:opaque:v1:session-root/45==".to_string()),
            step_progress: ConversationStepProgressDto {
                durable: None,
                live: None,
            },
            delta: ConversationDeltaDto::RehydrateRequired {
                error: ConversationErrorEnvelopeDto {
                    code: ConversationBannerErrorCodeDto::CursorExpired,
                    message: "cursor 已失效，请重新获取 snapshot".to_string(),
                    rehydrate_required: true,
                    details: Some(json!({ "cursor": "cursor:opaque:v1:session-root/12==" })),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&rehydrate_decoded).expect("rehydrate should encode"),
        rehydrate_fixture
    );
}

#[test]
fn conversation_plan_block_round_trips_with_review_details() {
    let block = ConversationBlockDto::Plan(ConversationPlanBlockDto {
        id: "plan-block-1".to_string(),
        turn_id: Some("turn-42".to_string()),
        tool_call_id: "call-plan-exit".to_string(),
        event_kind: ConversationPlanEventKindDto::ReviewPending,
        title: "Cleanup crates".to_string(),
        plan_path: "D:/demo/.astrcode/projects/demo/sessions/session-1/plan/cleanup-crates.md"
            .to_string(),
        summary: Some("正在做退出前自审".to_string()),
        status: None,
        slug: None,
        updated_at: None,
        content: None,
        review: Some(ConversationPlanReviewDto {
            kind: ConversationPlanReviewKindDto::FinalReview,
            checklist: vec![
                "Re-check assumptions against the code you already inspected.".to_string(),
            ],
        }),
        blockers: ConversationPlanBlockersDto {
            missing_headings: vec!["## Verification".to_string()],
            invalid_sections: vec![
                "session plan section '## Verification' must contain concrete actionable items"
                    .to_string(),
            ],
        },
    });

    let encoded = serde_json::to_value(&block).expect("plan block should encode");
    let decoded: ConversationBlockDto =
        serde_json::from_value(encoded.clone()).expect("plan block should decode");

    assert_eq!(decoded, block);
    assert_eq!(encoded["kind"], "plan");
    assert_eq!(encoded["eventKind"], "review_pending");
}

#[test]
fn conversation_system_note_round_trips_preserved_recent_turns() {
    let block = ConversationBlockDto::SystemNote(ConversationSystemNoteBlockDto {
        id: "system-compact-1".to_string(),
        note_kind: ConversationSystemNoteKindDto::Compact,
        markdown: "压缩摘要".to_string(),
        compact_meta: Some(ConversationLastCompactMetaDto {
            trigger: CompactTrigger::Auto,
            meta: CompactAppliedMeta {
                mode: CompactMode::Incremental,
                instructions_present: false,
                fallback_used: false,
                retry_count: 0,
                input_units: 3,
                output_summary_chars: 42,
            },
        }),
        preserved_recent_turns: Some(4),
    });

    let encoded = serde_json::to_value(&block).expect("system note should encode");
    let decoded: ConversationBlockDto =
        serde_json::from_value(encoded.clone()).expect("system note should decode");

    assert_eq!(decoded, block);
    assert_eq!(encoded["kind"], "system_note");
    assert_eq!(encoded["preservedRecentTurns"], 4);
}
