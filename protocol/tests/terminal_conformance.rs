use astrcode_protocol::http::{
    AgentLifecycleDto, ChildAgentRefDto, ChildSessionLineageKindDto, PhaseDto, ToolOutputStreamDto,
    terminal::v1::{
        TerminalAssistantBlockDto, TerminalBannerDto, TerminalBannerErrorCodeDto, TerminalBlockDto,
        TerminalBlockPatchDto, TerminalBlockStatusDto, TerminalChildHandoffBlockDto,
        TerminalChildHandoffKindDto, TerminalChildSummaryDto, TerminalControlStateDto,
        TerminalCursorDto, TerminalDeltaDto, TerminalErrorBlockDto, TerminalErrorEnvelopeDto,
        TerminalSlashActionKindDto, TerminalSlashCandidateDto, TerminalSnapshotResponseDto,
        TerminalStreamEnvelopeDto, TerminalSystemNoteBlockDto, TerminalSystemNoteKindDto,
        TerminalThinkingBlockDto, TerminalToolCallBlockDto, TerminalToolStreamBlockDto,
        TerminalTranscriptErrorCodeDto, TerminalUserBlockDto,
    },
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = match name {
        "snapshot" => include_str!("fixtures/terminal/v1/snapshot.json"),
        "delta_append_block" => include_str!("fixtures/terminal/v1/delta_append_block.json"),
        "delta_patch_block" => include_str!("fixtures/terminal/v1/delta_patch_block.json"),
        "delta_patch_tool_metadata" => {
            include_str!("fixtures/terminal/v1/delta_patch_tool_metadata.json")
        },
        "delta_patch_replace_markdown" => {
            include_str!("fixtures/terminal/v1/delta_patch_replace_markdown.json")
        },
        "delta_rehydrate_required" => {
            include_str!("fixtures/terminal/v1/delta_rehydrate_required.json")
        },
        "error_envelope" => include_str!("fixtures/terminal/v1/error_envelope.json"),
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(path).expect("fixture should be valid JSON")
}

fn sample_child_summary() -> TerminalChildSummaryDto {
    TerminalChildSummaryDto {
        child_session_id: "session-child-1".to_string(),
        child_agent_id: "agent-child-1".to_string(),
        title: "repo-inspector".to_string(),
        lifecycle: AgentLifecycleDto::Running,
        latest_output_summary: Some("正在扫描 crates/client".to_string()),
        child_ref: Some(ChildAgentRefDto {
            agent_id: "agent-child-1".to_string(),
            session_id: "session-child-1".to_string(),
            sub_run_id: "subrun-1".to_string(),
            parent_agent_id: Some("agent-root".to_string()),
            parent_sub_run_id: Some("subrun-root".to_string()),
            lineage_kind: ChildSessionLineageKindDto::Spawn,
            status: AgentLifecycleDto::Running,
            open_session_id: "session-root".to_string(),
        }),
    }
}

#[test]
fn terminal_snapshot_fixture_freezes_v1_hydration_shape() {
    let expected = TerminalSnapshotResponseDto {
        session_id: "session-root".to_string(),
        session_title: "Release terminal astrcode".to_string(),
        cursor: TerminalCursorDto("cursor:opaque:v1:session-root/42==".to_string()),
        phase: PhaseDto::Streaming,
        control: TerminalControlStateDto {
            phase: PhaseDto::Streaming,
            can_submit_prompt: false,
            can_request_compact: true,
            compact_pending: false,
            active_turn_id: Some("turn-42".to_string()),
        },
        blocks: vec![
            TerminalBlockDto::User(TerminalUserBlockDto {
                id: "block-user-1".to_string(),
                turn_id: Some("turn-42".to_string()),
                markdown: "请实现 terminal v1 协议骨架".to_string(),
            }),
            TerminalBlockDto::Thinking(TerminalThinkingBlockDto {
                id: "block-thinking-1".to_string(),
                turn_id: Some("turn-42".to_string()),
                status: TerminalBlockStatusDto::Streaming,
                markdown: "先冻结 wire contract，再继续 server/client。".to_string(),
            }),
            TerminalBlockDto::Assistant(TerminalAssistantBlockDto {
                id: "block-assistant-1".to_string(),
                turn_id: Some("turn-42".to_string()),
                status: TerminalBlockStatusDto::Streaming,
                markdown: "正在补 terminal v1 DTO。".to_string(),
            }),
            TerminalBlockDto::ToolCall(TerminalToolCallBlockDto {
                id: "block-tool-call-1".to_string(),
                turn_id: Some("turn-42".to_string()),
                tool_call_id: Some("tool-call-1".to_string()),
                tool_name: "shell_command".to_string(),
                status: TerminalBlockStatusDto::Complete,
                input: Some(json!({ "command": "rg terminal" })),
                summary: Some("读取 protocol 上下文".to_string()),
                metadata: None,
            }),
            TerminalBlockDto::ToolStream(TerminalToolStreamBlockDto {
                id: "block-tool-stream-1".to_string(),
                parent_tool_call_id: Some("tool-call-1".to_string()),
                stream: ToolOutputStreamDto::Stdout,
                status: TerminalBlockStatusDto::Complete,
                content: "protocol/http 目前没有 terminal namespace".to_string(),
            }),
            TerminalBlockDto::Error(TerminalErrorBlockDto {
                id: "block-error-1".to_string(),
                turn_id: Some("turn-41".to_string()),
                code: TerminalTranscriptErrorCodeDto::RateLimit,
                message: "本轮 provider rate limit".to_string(),
            }),
            TerminalBlockDto::SystemNote(TerminalSystemNoteBlockDto {
                id: "block-system-1".to_string(),
                note_kind: TerminalSystemNoteKindDto::Compact,
                markdown: "已应用 compact summary".to_string(),
            }),
            TerminalBlockDto::ChildHandoff(TerminalChildHandoffBlockDto {
                id: "block-child-1".to_string(),
                handoff_kind: TerminalChildHandoffKindDto::Delegated,
                child: sample_child_summary(),
                message: Some("让 child 检查 crates/client 设计".to_string()),
            }),
        ],
        child_summaries: vec![sample_child_summary()],
        slash_candidates: vec![
            TerminalSlashCandidateDto {
                id: "slash-new".to_string(),
                title: "/new".to_string(),
                description: "创建新会话".to_string(),
                keywords: vec!["session".to_string(), "create".to_string()],
                action_kind: TerminalSlashActionKindDto::ExecuteCommand,
                action_value: "new_session".to_string(),
            },
            TerminalSlashCandidateDto {
                id: "review".to_string(),
                title: "/review".to_string(),
                description: "调用 review skill".to_string(),
                keywords: vec!["skill".to_string(), "review".to_string()],
                action_kind: TerminalSlashActionKindDto::InsertText,
                action_value: "/review".to_string(),
            },
        ],
        banner: Some(TerminalBannerDto {
            error: TerminalErrorEnvelopeDto {
                code: TerminalBannerErrorCodeDto::StreamDisconnected,
                message: "stream 已中断，正在重连".to_string(),
                rehydrate_required: false,
                details: Some(json!({ "retryAt": "2026-04-15T03:40:00Z" })),
            },
        }),
    };

    let fixture = fixture("snapshot");
    let decoded: TerminalSnapshotResponseDto =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("snapshot should encode"),
        fixture
    );
    assert_eq!(decoded.cursor.0, "cursor:opaque:v1:session-root/42==");
}

#[test]
fn terminal_delta_fixtures_freeze_append_patch_and_rehydrate_shapes() {
    let append_fixture = fixture("delta_append_block");
    let append_decoded: TerminalStreamEnvelopeDto =
        serde_json::from_value(append_fixture.clone()).expect("append fixture should decode");
    assert_eq!(
        append_decoded,
        TerminalStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: TerminalCursorDto("cursor:opaque:v1:session-root/43==".to_string()),
            delta: TerminalDeltaDto::AppendBlock {
                block: TerminalBlockDto::Assistant(TerminalAssistantBlockDto {
                    id: "block-assistant-1".to_string(),
                    turn_id: Some("turn-42".to_string()),
                    status: TerminalBlockStatusDto::Streaming,
                    markdown: "terminal stream 已经接上。".to_string(),
                }),
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&append_decoded).expect("append should encode"),
        append_fixture
    );

    let patch_fixture = fixture("delta_patch_block");
    let patch_decoded: TerminalStreamEnvelopeDto =
        serde_json::from_value(patch_fixture.clone()).expect("patch fixture should decode");
    assert_eq!(
        patch_decoded,
        TerminalStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: TerminalCursorDto("cursor:opaque:v1:session-root/44==".to_string()),
            delta: TerminalDeltaDto::PatchBlock {
                block_id: "block-tool-stream-1".to_string(),
                patch: TerminalBlockPatchDto::AppendToolStream {
                    stream: ToolOutputStreamDto::Stderr,
                    chunk: "line 1\nline 2".to_string(),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&patch_decoded).expect("patch should encode"),
        patch_fixture
    );

    let tool_metadata_fixture = fixture("delta_patch_tool_metadata");
    let tool_metadata_decoded: TerminalStreamEnvelopeDto =
        serde_json::from_value(tool_metadata_fixture.clone())
            .expect("tool metadata patch fixture should decode");
    assert_eq!(
        tool_metadata_decoded,
        TerminalStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: TerminalCursorDto("cursor:opaque:v1:session-root/44.5==".to_string()),
            delta: TerminalDeltaDto::PatchBlock {
                block_id: "block-tool-call-1".to_string(),
                patch: TerminalBlockPatchDto::ReplaceMetadata {
                    metadata: json!({
                        "openSessionId": "session-child-1",
                        "agentRef": {
                            "agentId": "agent-child-1",
                            "subRunId": "subrun-1",
                            "openSessionId": "session-child-1"
                        }
                    }),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&tool_metadata_decoded).expect("tool metadata patch should encode"),
        tool_metadata_fixture
    );

    let replace_fixture = fixture("delta_patch_replace_markdown");
    let replace_decoded: TerminalStreamEnvelopeDto =
        serde_json::from_value(replace_fixture.clone()).expect("replace fixture should decode");
    assert_eq!(
        replace_decoded,
        TerminalStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: TerminalCursorDto("cursor:opaque:v1:session-root/44.1==".to_string()),
            delta: TerminalDeltaDto::PatchBlock {
                block_id: "block-thinking-1".to_string(),
                patch: TerminalBlockPatchDto::ReplaceMarkdown {
                    markdown: "provider rewrote the full reasoning".to_string(),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&replace_decoded).expect("replace should encode"),
        replace_fixture
    );

    let reset_fixture = fixture("delta_rehydrate_required");
    let reset_decoded: TerminalStreamEnvelopeDto =
        serde_json::from_value(reset_fixture.clone()).expect("reset fixture should decode");
    assert_eq!(
        reset_decoded,
        TerminalStreamEnvelopeDto {
            session_id: "session-root".to_string(),
            cursor: TerminalCursorDto("cursor:opaque:v1:session-root/45==".to_string()),
            delta: TerminalDeltaDto::RehydrateRequired {
                error: TerminalErrorEnvelopeDto {
                    code: TerminalBannerErrorCodeDto::CursorExpired,
                    message: "cursor 已失效，请重新获取 snapshot".to_string(),
                    rehydrate_required: true,
                    details: Some(json!({ "cursor": "cursor:opaque:v1:session-root/12==" })),
                },
            },
        }
    );
    assert_eq!(
        serde_json::to_value(&reset_decoded).expect("rehydrate should encode"),
        reset_fixture
    );
}

#[test]
fn terminal_error_envelope_fixture_freezes_banner_error_contract() {
    let expected = TerminalErrorEnvelopeDto {
        code: TerminalBannerErrorCodeDto::AuthExpired,
        message: "token 已过期，请重新 exchange".to_string(),
        rehydrate_required: false,
        details: Some(json!({
            "origin": "http://127.0.0.1:5529",
            "status": 401
        })),
    };

    let fixture = fixture("error_envelope");
    let decoded: TerminalErrorEnvelopeDto =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("error envelope should encode"),
        fixture
    );
}
