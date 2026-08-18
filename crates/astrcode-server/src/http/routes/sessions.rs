//! Session 生命周期与对话快照路由。

use astrcode_core::{
    tool::{SessionApiError, SessionToolSelection},
    types::SessionId,
};
use astrcode_protocol::http::{
    CommandCompletionItemDto, CommandCompletionRequest, CommandCompletionResponse,
    CommandInvokeRequest, CommandInvokeResponse, CompactSessionRequest, CompactSessionResponse,
    ConfigureSessionToolsRequest, ConfigureSessionToolsResponse, ConversationCursorDto,
    ConversationItemsPageResponseDto, ConversationTimelineCursorDto, CreateSessionRequest,
    CreateSessionResponseDto, DeleteProjectResponseDto, PromptRequest, PromptSubmitResponse,
    SessionListItemDto, SessionListResponseDto, SlashCommandListResponseDto, ToolApprovalRequest,
    ToolSelectionDto,
};
use astrcode_session::compaction::ManualCompactionOutcome;
use astrcode_session_projection::SessionSummary;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::super::{
    HttpState, bad_request_response, conflict_response,
    conversation_timeline::{
        ConversationTimelineError, DEFAULT_PAGE_ITEMS, MAX_PAGE_BYTES, MAX_PAGE_ITEMS, PageBudget,
        TimelineCursor,
    },
    handler_error_response, internal_error_response, not_found_response,
    projection::{
        session_title_from_working_dir,
        snapshot::{conversation_state_to_dto, conversation_to_dto, decorate_timeline_items},
    },
};
use crate::{
    protocol_mapping::{command_info_to_dto, keybinding_to_dto, status_item_to_dto},
    session_command_contract::{CommandInvocation, HandlerError, PromptSubmission},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::http) struct DeleteProjectParams {
    working_dir: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::http) struct ConversationItemsParams {
    before: Option<String>,
    limit: Option<usize>,
}

pub(in crate::http) async fn create_session(
    State(state): State<HttpState>,
    Json(request): Json<CreateSessionRequest>,
) -> Response {
    let tool_selection = match request.tool_selection.map(map_tool_selection).transpose() {
        Ok(selection) => selection,
        Err(message) => return bad_request_response("invalid_tool_selection", message),
    };
    tracing::info!(working_dir = %request.working_dir, "POST /api/sessions — create_session");
    match state
        .app
        .session_commands()
        .create_session(request.working_dir, tool_selection)
        .await
    {
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

pub(in crate::http) async fn configure_session_tools(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<ConfigureSessionToolsRequest>,
) -> Response {
    let selection = match map_tool_selection(request.selection) {
        Ok(selection) => selection,
        Err(message) => return bad_request_response("invalid_tool_selection", message),
    };
    match state
        .app
        .session_commands()
        .configure_tools(SessionId::from(session_id), selection)
        .await
    {
        Ok(effective) => Json(ConfigureSessionToolsResponse {
            selection: effective.into(),
        })
        .into_response(),
        Err(error) => handler_error_response(error, "configure_tools_failed"),
    }
}

fn map_tool_selection(selection: ToolSelectionDto) -> Result<SessionToolSelection, String> {
    match selection {
        ToolSelectionDto::All { except } => Ok(SessionToolSelection::All {
            except: normalized_tool_names(except)?,
        }),
        ToolSelectionDto::Only { names } => Ok(SessionToolSelection::Only {
            names: normalized_tool_names(names)?,
        }),
    }
}

fn normalized_tool_names(names: Vec<String>) -> Result<Vec<String>, String> {
    astrcode_core::tool::validated_tool_names(names).map_err(|error| error.to_string())
}

pub(in crate::http) async fn list_sessions(State(state): State<HttpState>) -> Response {
    match state.app.runtime().session_manager().list_summaries().await {
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
    match state
        .app
        .runtime()
        .session_manager()
        .read_model(&session_id)
        .await
    {
        Ok(snapshot) => {
            let streaming = state.app.event_bus().streaming_snapshot(&session_id);
            Json(conversation_to_dto(&snapshot, streaming.as_ref())).into_response()
        },
        Err(error) => not_found_response("session_not_found", error),
    }
}

pub(in crate::http) async fn conversation_state(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .app
        .runtime()
        .session_manager()
        .read_model(&session_id)
        .await
    {
        Ok(snapshot) => {
            let streaming = state.app.event_bus().streaming_snapshot(&session_id);
            Json(conversation_state_to_dto(&snapshot, streaming.as_ref())).into_response()
        },
        Err(error) => not_found_response("session_not_found", error),
    }
}

pub(in crate::http) async fn conversation_items(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Query(params): Query<ConversationItemsParams>,
) -> Response {
    let session_id = SessionId::from(session_id);
    let before = match params.before.map(TimelineCursor::parse).transpose() {
        Ok(cursor) => cursor,
        Err(error) => return bad_request_response("invalid_timeline_cursor", error),
    };
    let snapshot = match state
        .app
        .runtime()
        .session_manager()
        .read_model(&session_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return not_found_response("session_not_found", error),
    };
    let mut page = match state
        .conversation_timeline
        .page_before(
            &session_id,
            before.as_ref(),
            PageBudget {
                max_items: params
                    .limit
                    .unwrap_or(DEFAULT_PAGE_ITEMS)
                    .clamp(1, MAX_PAGE_ITEMS),
                max_bytes: MAX_PAGE_BYTES,
            },
        )
        .await
    {
        Ok(page) => page,
        Err(ConversationTimelineError::InvalidCursor) => {
            return bad_request_response(
                "invalid_timeline_cursor",
                "invalid conversation timeline cursor",
            );
        },
        Err(ConversationTimelineError::Storage(astrcode_storage::StorageError::NotFound(
            error,
        ))) => return not_found_response("session_not_found", error),
        Err(error) => return internal_error_response("conversation_timeline_failed", error),
    };
    decorate_timeline_items(&mut page.items, &snapshot, before.is_none());

    Json(ConversationItemsPageResponseDto {
        items: page.items,
        older_cursor: page
            .older_cursor
            .map(|cursor| ConversationTimelineCursorDto {
                value: cursor.into_string(),
            }),
        has_older: page.has_older,
        snapshot_cursor: ConversationCursorDto {
            value: snapshot.cursor(),
        },
    })
    .into_response()
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
        .app
        .session_commands()
        .inject_input(session_id.clone(), request.text)
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
    let Some(ops) = state.app.runtime().runtime_services().session_ops() else {
        return internal_error_response(
            "session_ops_unavailable",
            "session operations unavailable",
        );
    };
    match ops
        .resolve_tool_approval(&session_id_str, &request.call_id, request.decision.into())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => approval_error_response(error),
    }
}

fn approval_error_response(error: SessionApiError) -> Response {
    match error {
        SessionApiError::NotFound(message) => not_found_response("approval_not_pending", message),
        SessionApiError::SessionBusy(message) => conflict_response("approval_unavailable", message),
        error => internal_error_response("approval_failed", error),
    }
}

pub(in crate::http) async fn submit_prompt(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Response {
    let attachments: Vec<_> = request.attachments.into_iter().map(Into::into).collect();
    if let Err(error) = astrcode_core::message_attachment::validate_attachments(&attachments) {
        return handler_error_response(
            HandlerError::InvalidRequest(error.to_string()),
            "prompt_failed",
        );
    }
    tracing::info!(
        session_id = %session_id,
        text_len = request.text.len(),
        attachment_count = attachments.len(),
        "POST prompt submit"
    );
    let session_id = SessionId::from(session_id);
    let result = state
        .app
        .session_commands()
        .submit_input(
            session_id.clone(),
            astrcode_core::user_input::UserInput {
                text: request.text,
                attachments,
            },
        )
        .await;
    match result {
        Ok(PromptSubmission::Accepted { turn_id }) => {
            tracing::info!(session_id = %session_id, turn_id = %turn_id, "prompt accepted");
            Json(PromptSubmitResponse::Accepted {
                session_id: session_id.into_string(),
                turn_id: turn_id.into_string(),
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

pub(in crate::http) async fn invoke_command(
    State(state): State<HttpState>,
    Path((session_id, name)): Path<(String, String)>,
    Json(request): Json<CommandInvokeRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .app
        .session_commands()
        .invoke_named_command(session_id.clone(), name, request.arguments)
        .await
    {
        Ok(CommandInvocation::Display { content, is_error }) => {
            Json(CommandInvokeResponse::Display {
                session_id: session_id.into_string(),
                content,
                is_error,
            })
            .into_response()
        },
        Ok(CommandInvocation::Handled { message }) => Json(CommandInvokeResponse::Handled {
            session_id: session_id.into_string(),
            message,
        })
        .into_response(),
        Ok(CommandInvocation::Started { turn_id }) => Json(CommandInvokeResponse::Started {
            session_id: session_id.into_string(),
            turn_id: turn_id.into_string(),
        })
        .into_response(),
        Err(HandlerError::UnknownCommand(cmd)) => {
            handler_error_response(HandlerError::UnknownCommand(cmd), "command_execute_failed")
        },
        Err(error) => handler_error_response(error, "command_execute_failed"),
    }
}

pub(in crate::http) async fn complete_command(
    State(state): State<HttpState>,
    Path((session_id, name)): Path<(String, String)>,
    Json(request): Json<CommandCompletionRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .app
        .session_commands()
        .complete_command(session_id, name, request.argument, request.cursor)
        .await
    {
        Ok(completions) => Json(CommandCompletionResponse {
            items: completions
                .items
                .into_iter()
                .map(|item| CommandCompletionItemDto {
                    label: item.label,
                    insert_text: item.insert_text,
                    detail: item.detail,
                })
                .collect(),
            truncated: completions.truncated,
        })
        .into_response(),
        Err(error) => handler_error_response(error, "command_complete_failed"),
    }
}

pub(in crate::http) async fn list_commands(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .app
        .session_commands()
        .command_list(&session_id, false)
        .await
    {
        Ok(command_list) => {
            let keybindings = command_list
                .keybindings
                .into_iter()
                .map(keybinding_to_dto)
                .collect();
            let status_items = command_list
                .status_items
                .into_iter()
                .map(status_item_to_dto)
                .collect();
            Json(SlashCommandListResponseDto {
                commands: command_list
                    .commands
                    .into_iter()
                    .map(command_info_to_dto)
                    .collect(),
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
        .app
        .session_commands()
        .compact_session(&session_id, request.keep_recent_turns)
        .await
    {
        Ok(ManualCompactionOutcome::Compacted { messages_removed }) => {
            Json(CompactSessionResponse {
                compacted: true,
                message: format!("compact completed; {messages_removed} messages removed"),
            })
            .into_response()
        },
        Ok(ManualCompactionOutcome::Skipped { message }) => Json(CompactSessionResponse {
            compacted: false,
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
    match state
        .app
        .session_commands()
        .abort_session(&session_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => handler_error_response(error, "abort_failed"),
    }
}

pub(in crate::http) async fn delete_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .app
        .session_commands()
        .delete_session(&session_id)
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
    let at_cursor = request.storage_seq.map(|seq| seq.to_string());
    match state
        .app
        .session_commands()
        .fork_session(source_id, at_cursor)
        .await
    {
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
    match state
        .app
        .session_commands()
        .delete_project(&params.working_dir)
        .await
    {
        Ok(deleted_count) => Json(DeleteProjectResponseDto { deleted_count }).into_response(),
        Err(error) => internal_error_response("delete_failed", error),
    }
}

fn summary_to_dto(summary: SessionSummary) -> SessionListItemDto {
    let title = summary
        .first_user_message
        .clone()
        .unwrap_or_else(|| session_title_from_working_dir(&summary.working_dir));
    SessionListItemDto {
        session_id: summary.session_id.into_string(),
        working_dir: summary.working_dir,
        title,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        phase: summary.phase.into(),
        first_user_message: summary.first_user_message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_errors_preserve_not_found_and_busy_statuses() {
        assert_eq!(
            approval_error_response(SessionApiError::NotFound("missing".into())).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            approval_error_response(SessionApiError::SessionBusy("dropped".into())).status(),
            StatusCode::CONFLICT
        );
    }
}
