//! Session 生命周期与对话快照路由。

use astrcode_core::{
    storage::{SessionReadModel, SessionSummary},
    types::SessionId,
};
use astrcode_protocol::{
    commands::ClientCommand,
    http::{
        AgentSessionLinkDto, CompactSessionRequest, CompactSessionResponse, ConversationBlockDto,
        ConversationBlockStatusDto, ConversationCursorDto, ConversationSnapshotResponseDto,
        CreateSessionRequest, CreateSessionResponseDto, DeleteProjectResponseDto,
        ExecuteExtensionCommandRequest, PromptRequest, PromptSubmitResponse, SessionListItemDto,
        SessionListResponseDto, SlashCommandListResponseDto, ToolApprovalRequest,
        ToolUiRespondRequest, ToolUiRespondResponse,
    },
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::super::{
    HttpState, handler_error_response, internal_error_response, not_found_response,
    projection::{
        blocks::{compact_summary_block, latest_compact_boundary, messages_to_blocks},
        live::control_from_phase,
    },
};
use crate::{
    handler::{HandlerError, ManualCompactOutcome, PromptSubmission},
    server_event_bus::StreamingSnapshot,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::http) struct DeleteProjectParams {
    working_dir: String,
}

pub(in crate::http) async fn create_session(
    State(state): State<HttpState>,
    Json(request): Json<CreateSessionRequest>,
) -> Response {
    tracing::info!(working_dir = %request.working_dir, "POST /api/sessions — create_session");
    match state.handler.create_session(request.working_dir).await {
        Ok(session_id) => {
            tracing::info!(session_id = %session_id, "session created");
            Json(CreateSessionResponseDto {
                session_id: session_id.into_string(),
            })
            .into_response()
        },
        Err(error) => {
            tracing::error!(error = %error, "create_session failed");
            internal_error_response("create_failed", error)
        },
    }
}

pub(in crate::http) async fn list_sessions(State(state): State<HttpState>) -> Response {
    match state.runtime.session_manager().list_summaries().await {
        Ok(summaries) => Json(SessionListResponseDto {
            sessions: summaries.into_iter().map(summary_to_dto).collect(),
        })
        .into_response(),
        Err(error) => internal_error_response("list_failed", error),
    }
}

pub(in crate::http) async fn conversation_snapshot(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    // 主动修复进程重启后残留的过期 turn phase（如 CallingTool / Thinking），
    // 使前端打开 session 时就能看到正确的 Idle 状态。
    if let Err(e) = state.handler.repair_stale_turn(session_id.clone()).await {
        if !matches!(e, HandlerError::NoActiveTurn) {
            tracing::warn!(session_id = %session_id, error = %e, "stale turn repair failed in snapshot");
        }
    }
    match state
        .runtime
        .session_manager()
        .read_model(&session_id)
        .await
    {
        Ok(snapshot) => {
            let streaming = state.event_bus.streaming_snapshot(&session_id);
            Json(conversation_to_dto(snapshot, streaming.as_ref())).into_response()
        },
        Err(error) => not_found_response("session_not_found", error),
    }
}

pub(in crate::http) async fn inject_message(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Response {
    tracing::info!(
        session_id = %session_id,
        text_len = request.text.len(),
        "POST inject"
    );
    let session_id = SessionId::from(session_id);
    match state
        .handler
        .inject_input_for_session(session_id.clone(), request.text)
        .await
    {
        Ok(PromptSubmission::Handled { message }) => Json(PromptSubmitResponse::Handled {
            session_id: session_id.into_string(),
            message,
        })
        .into_response(),
        Ok(PromptSubmission::Accepted { turn_id }) => {
            tracing::info!(session_id = %session_id, turn_id = %turn_id, "inject started turn");
            Json(PromptSubmitResponse::Accepted {
                session_id: session_id.into_string(),
                turn_id: turn_id.into_string(),
                branched_from_session_id: None,
            })
            .into_response()
        },
        Err(HandlerError::NoActiveTurn) => {
            tracing::warn!(session_id = %session_id, "inject rejected: no active turn");
            handler_error_response(HandlerError::NoActiveTurn, "inject_failed")
        },
        Err(error) => {
            tracing::error!(session_id = %session_id, error = %error, "inject failed");
            handler_error_response(error, "inject_failed")
        },
    }
}

pub(in crate::http) async fn resolve_tool_approval(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<ToolApprovalRequest>,
) -> Response {
    let session_id_str = session_id.clone();
    let Some(ops) = state.runtime.capabilities().session_ops() else {
        return internal_error_response(
            "session_ops_unavailable",
            "session operations unavailable",
        );
    };
    match ops
        .resolve_tool_approval(&session_id_str, &request.call_id, request.decision)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => handler_error_response(
            HandlerError::SessionNotFound(error.to_string()),
            "approval_failed",
        ),
    }
}

pub(in crate::http) async fn submit_tool_ui_respond(
    State(state): State<HttpState>,
    Path((session_id, call_id)): Path<(String, String)>,
    Json(request): Json<ToolUiRespondRequest>,
) -> Response {
    let session_id_str = session_id.clone();
    let Some(ops) = state.runtime.capabilities().session_ops() else {
        return internal_error_response(
            "session_ops_unavailable",
            "session operations unavailable",
        );
    };
    if request.answers.is_empty() {
        return handler_error_response(
            HandlerError::InvalidRequest("answers must not be empty".into()),
            "tool_ui_respond_failed",
        );
    }
    match ops
        .resolve_tool_ui_response(&session_id_str, &call_id, request.answers)
        .await
    {
        Ok(()) => Json(ToolUiRespondResponse { accepted: true }).into_response(),
        Err(error) => handler_error_response(
            HandlerError::SessionNotFound(error.to_string()),
            "tool_ui_respond_failed",
        ),
    }
}

pub(in crate::http) async fn submit_prompt(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Response {
    tracing::info!(session_id = %session_id, text_len = request.text.len(), "POST prompt submit");
    let session_id = SessionId::from(session_id);
    let result = state
        .handler
        .submit_input_for_session(session_id.clone(), request.text)
        .await;
    match result {
        Ok(PromptSubmission::Accepted { turn_id }) => {
            tracing::info!(session_id = %session_id, turn_id = %turn_id, "prompt accepted");
            Json(PromptSubmitResponse::Accepted {
                session_id: session_id.into_string(),
                turn_id: turn_id.into_string(),
                branched_from_session_id: None,
            })
            .into_response()
        },
        Ok(PromptSubmission::Handled { message }) => Json(PromptSubmitResponse::Handled {
            session_id: session_id.into_string(),
            message,
        })
        .into_response(),
        Err(HandlerError::TurnAlreadyRunning) => {
            tracing::warn!(session_id = %session_id, "prompt rejected: turn already running");
            handler_error_response(HandlerError::TurnAlreadyRunning, "prompt_failed")
        },
        Err(HandlerError::UnknownCommand(cmd)) => {
            tracing::warn!(session_id = %session_id, command = %cmd, "prompt rejected: unknown slash command");
            handler_error_response(HandlerError::UnknownCommand(cmd), "prompt_failed")
        },
        Err(error) => {
            tracing::error!(session_id = %session_id, error = %error, "prompt failed");
            handler_error_response(error, "prompt_failed")
        },
    }
}

pub(in crate::http) async fn execute_extension_command(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<ExecuteExtensionCommandRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    let command_name = request.command.trim().to_ascii_lowercase();
    if command_name.is_empty() {
        return handler_error_response(
            HandlerError::InvalidRequest("command must not be empty".into()),
            "command_execute_failed",
        );
    }
    let visible_text = if request.arguments.trim().is_empty() {
        format!("/{command_name}")
    } else {
        format!("/{command_name} {}", request.arguments.trim())
    };
    match state
        .handler
        .submit_input_for_session(session_id.clone(), visible_text)
        .await
    {
        Ok(PromptSubmission::Handled { message }) => Json(PromptSubmitResponse::Handled {
            session_id: session_id.into_string(),
            message,
        })
        .into_response(),
        Ok(PromptSubmission::Accepted { turn_id }) => Json(PromptSubmitResponse::Accepted {
            session_id: session_id.into_string(),
            turn_id: turn_id.into_string(),
            branched_from_session_id: None,
        })
        .into_response(),
        Err(HandlerError::UnknownCommand(cmd)) => {
            handler_error_response(HandlerError::UnknownCommand(cmd), "command_execute_failed")
        },
        Err(error) => handler_error_response(error, "command_execute_failed"),
    }
}

pub(in crate::http) async fn list_commands(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.command_infos_for_session(session_id).await {
        Ok(commands) => {
            use astrcode_protocol::http::{KeybindingDto, StatusItemDto};
            let keybindings: Vec<KeybindingDto> = state
                .runtime
                .extension_runner()
                .collect_keybindings()
                .into_iter()
                .map(|kb| KeybindingDto {
                    key: kb.key,
                    command: kb.command,
                    arguments: kb.arguments,
                    description: kb.description,
                })
                .collect();
            let status_items: Vec<StatusItemDto> = state
                .runtime
                .extension_runner()
                .collect_status_items()
                .into_iter()
                .map(|item| StatusItemDto {
                    id: item.id,
                    text: item.text,
                    priority: item.priority,
                })
                .collect();
            Json(SlashCommandListResponseDto {
                commands: commands.into_iter().map(Into::into).collect(),
                keybindings,
                status_items,
            })
            .into_response()
        },
        Err(error) => not_found_response("session_not_found", error),
    }
}

pub(in crate::http) async fn compact_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<CompactSessionRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .handler
        .compact_session(session_id, request.keep_recent_turns)
        .await
    {
        Ok(ManualCompactOutcome::Compacted { session_id }) => Json(CompactSessionResponse {
            accepted: true,
            deferred: false,
            new_session_id: Some(session_id.into_string()),
            message: "compact accepted".into(),
        })
        .into_response(),
        Ok(ManualCompactOutcome::Skipped { message }) => Json(CompactSessionResponse {
            accepted: false,
            deferred: false,
            new_session_id: None,
            message,
        })
        .into_response(),
        Err(error) => handler_error_response(error, "compact_failed"),
    }
}

pub(in crate::http) async fn abort_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.abort_session(session_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => handler_error_response(error, "abort_failed"),
    }
}

pub(in crate::http) async fn delete_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    match state
        .handler
        .handle(ClientCommand::DeleteSession { session_id })
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => not_found_response("delete_failed", error),
    }
}

pub(in crate::http) async fn fork_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<astrcode_protocol::http::ForkSessionRequest>,
) -> Response {
    tracing::info!(session_id = %session_id, "POST fork session");
    let source_id = SessionId::from(session_id);
    let at_cursor = request
        .storage_seq
        .map(|seq| seq.to_string())
        .or(request.turn_id);
    match state.handler.fork_session(source_id, at_cursor).await {
        Ok(new_session_id) => Json(CreateSessionResponseDto {
            session_id: new_session_id.into_string(),
        })
        .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "fork_session failed");
            handler_error_response(error, "fork_failed")
        },
    }
}

pub(in crate::http) async fn delete_project(
    State(state): State<HttpState>,
    Query(params): Query<DeleteProjectParams>,
) -> Response {
    match state.handler.delete_project(params.working_dir).await {
        Ok(deleted_count) => Json(DeleteProjectResponseDto { deleted_count }).into_response(),
        Err(error) => internal_error_response("delete_failed", error),
    }
}

fn summary_to_dto(summary: SessionSummary) -> SessionListItemDto {
    let title = summary
        .first_user_message
        .clone()
        .unwrap_or_else(|| session_title(&summary.working_dir));
    SessionListItemDto {
        session_id: summary.session_id.into_string(),
        working_dir: summary.working_dir,
        display_name: title.clone(),
        title,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        parent_session_id: summary.parent_session_id.map(SessionId::into_string),
        parent_storage_seq: None,
        phase: summary.phase,
        first_user_message: summary.first_user_message,
        source_extension: summary.source_extension,
    }
}

fn conversation_to_dto(
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
