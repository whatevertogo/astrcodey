//! Session 生命周期与对话快照路由。

use astrcode_core::{
    event::Phase,
    storage::{SessionReadModel, SessionSummary},
    types::SessionId,
};
use astrcode_protocol::{
    commands::ClientCommand,
    http::{
        CompactSessionRequest, CompactSessionResponse, ConversationBlockDto,
        ConversationControlStateDto, ConversationCursorDto, ConversationSnapshotResponseDto,
        CreateSessionRequest, CreateSessionResponseDto, DeleteProjectResponseDto,
        HttpAgentSessionLinkDto, PromptRequest, PromptSubmitResponse, SessionListItemDto,
        SessionListResponseDto, SlashCommandListResponseDto,
    },
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::super::{HttpState, error_response, projection::blocks::messages_to_blocks};
use crate::handler::{HandlerError, ManualCompactOutcome, PromptSubmission, snapshot};

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
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "create_failed", error)
        },
    }
}

pub(in crate::http) async fn list_sessions(State(state): State<HttpState>) -> Response {
    match state.runtime.session_manager.list_summaries().await {
        Ok(summaries) => Json(SessionListResponseDto {
            sessions: summaries.into_iter().map(summary_to_dto).collect(),
        })
        .into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "list_failed", error),
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
    match state.runtime.session_manager.read_model(&session_id).await {
        Ok(snapshot) => Json(conversation_to_dto(snapshot)).into_response(),
        Err(error) => error_response(StatusCode::NOT_FOUND, "session_not_found", error),
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
            error_response(
                StatusCode::CONFLICT,
                "turn_running",
                "A turn is already running",
            )
        },
        Err(HandlerError::UnknownCommand(cmd)) => {
            tracing::warn!(session_id = %session_id, command = %cmd, "prompt rejected: unknown slash command");
            error_response(
                StatusCode::BAD_REQUEST,
                "unknown_command",
                format!("Unknown command: /{cmd}"),
            )
        },
        Err(error) => {
            tracing::error!(session_id = %session_id, error = %error, "prompt failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "prompt_failed",
                error.to_string(),
            )
        },
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
                .extension_runner
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
                .extension_runner
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
        Err(error) => error_response(StatusCode::NOT_FOUND, "session_not_found", error),
    }
}

pub(in crate::http) async fn compact_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(_request): Json<CompactSessionRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.compact_session(session_id).await {
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
        Err(error) if matches!(error, HandlerError::CompactBlocked) => {
            error_response(StatusCode::CONFLICT, "turn_running", error.to_string())
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "compact_failed",
            error.to_string(),
        ),
    }
}

pub(in crate::http) async fn abort_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.abort_session(session_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if matches!(error, HandlerError::NoActiveTurn) => {
            error_response(StatusCode::NOT_FOUND, "no_active_turn", error.to_string())
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "abort_failed",
            error.to_string(),
        ),
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
        Err(error) => error_response(StatusCode::NOT_FOUND, "delete_failed", error),
    }
}

pub(in crate::http) async fn delete_project(
    State(state): State<HttpState>,
    Query(params): Query<DeleteProjectParams>,
) -> Response {
    match state.runtime.session_manager.list_summaries().await {
        Ok(summaries) => {
            let matching: Vec<_> = summaries
                .into_iter()
                .filter(|s| s.working_dir == params.working_dir)
                .collect();
            let mut deleted_count = 0usize;
            for summary in &matching {
                match state
                    .handler
                    .handle(ClientCommand::DeleteSession {
                        session_id: summary.session_id.to_string(),
                    })
                    .await
                {
                    Ok(()) => deleted_count += 1,
                    Err(error) => {
                        tracing::warn!(
                            session_id = %summary.session_id,
                            error = %error,
                            "delete_project: failed to delete session, continuing"
                        );
                    },
                }
            }
            Json(DeleteProjectResponseDto { deleted_count }).into_response()
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "list_failed", error),
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
        source_plugin: summary.source_plugin,
    }
}

fn conversation_to_dto(session: SessionReadModel) -> ConversationSnapshotResponseDto {
    let can_submit_prompt = matches!(session.phase, Phase::Idle | Phase::Error);
    let title = session
        .first_user_message()
        .unwrap_or_else(|| session_title(&session.working_dir));

    let mut blocks = messages_to_blocks(&session.messages, &session.background_tool_calls);
    for boundary in &session.compact_boundaries {
        blocks.push(ConversationBlockDto::CompactSummary {
            id: format!("compact-{}", boundary.seq),
            summary: boundary.summary.clone(),
            trigger: boundary.trigger.clone(),
            pre_tokens: boundary.pre_tokens,
            post_tokens: boundary.post_tokens,
            transcript_path: boundary.transcript_path.clone(),
        });
    }

    ConversationSnapshotResponseDto {
        session_id: session.session_id.to_string(),
        session_title: title,
        cursor: ConversationCursorDto {
            value: session.cursor(),
        },
        phase: session.phase,
        control: ConversationControlStateDto {
            phase: session.phase,
            can_submit_prompt,
            can_request_compact: can_submit_prompt && !session.messages.is_empty(),
            compact_pending: false,
            compacting: matches!(session.phase, Phase::Compacting),
            current_mode_id: None,
            active_turn_id: None,
        },
        blocks,
        agent_sessions: session
            .agent_sessions
            .iter()
            .map(|link| HttpAgentSessionLinkDto {
                child_session_id: link.child_session_id.to_string(),
                agent_name: link.agent_name.clone(),
                task: link.task.clone(),
                status: snapshot::agent_status_to_dto(link.status),
            })
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
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole},
        storage::BackgroundToolCallView,
    };
    use astrcode_protocol::http::ConversationBlockStatusDto;

    use super::*;

    #[test]
    fn conversation_snapshot_cursor_is_full_snapshot_version() {
        let mut session = SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(9);
        session.messages.push(LlmMessage::user("hello"));

        let dto = conversation_to_dto(session);

        assert_eq!(dto.cursor.value, "9");
        assert_eq!(dto.blocks.len(), 1);
    }

    #[test]
    fn conversation_snapshot_renders_tool_call_as_structured_block() {
        let mut session = SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.messages.push(LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "tool-1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "Cargo.toml" }),
            }],
            name: None,
            reasoning_content: None,
        });
        session
            .messages
            .push(LlmMessage::tool("read", "tool-1", "file contents", false));

        let dto = conversation_to_dto(session);

        assert_eq!(dto.blocks.len(), 1);
        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                id,
                name,
                arguments,
                text,
                status,
                task_id: _,
                metadata: _,
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
    fn conversation_snapshot_restores_background_task_state() {
        let mut session = SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.messages.push(LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "tool-1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({ "command": "npm run dev" }),
            }],
            name: None,
            reasoning_content: None,
        });
        session.messages.push(LlmMessage::tool(
            "shell",
            "tool-1",
            "Task moved to background (task: bg-1).",
            false,
        ));
        session.background_tool_calls.insert(
            "tool-1".into(),
            BackgroundToolCallView {
                task_id: "bg-1".into(),
                completed: false,
            },
        );

        let dto = conversation_to_dto(session);

        match &dto.blocks[0] {
            ConversationBlockDto::ToolCall {
                status, task_id, ..
            } => {
                assert!(matches!(status, ConversationBlockStatusDto::Backgrounded));
                assert_eq!(task_id.as_deref(), Some("bg-1"));
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }
}
