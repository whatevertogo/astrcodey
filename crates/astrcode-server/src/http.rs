//! Axum HTTP/SSE 入口。
//!
//! 这层只做 wire 适配：命令统一进入 [`CommandHandler`]，读接口从 storage
//! read model 映射到 `astrcode_protocol::http` DTO。

use std::{convert::Infallible, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::{ConversationReadModel, SessionSummary},
    types::SessionId,
};
use astrcode_protocol::{
    events::ClientNotification,
    http::{
        CompactSessionRequest, CompactSessionResponse, ConversationBlockDto,
        ConversationBlockStatusDto, ConversationControlStateDto, ConversationCursorDto,
        ConversationDeltaDto, ConversationErrorEnvelopeDto, ConversationSnapshotResponseDto,
        ConversationStreamEnvelopeDto, CreateSessionRequest, CreateSessionResponseDto,
        ForkSessionRequest, PromptRequest, PromptSubmitResponse, SessionListItemDto,
        SessionListResponseDto,
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, stream};
use tokio::sync::{Mutex, broadcast};

use crate::{bootstrap::ServerRuntime, handler::CommandHandler};

/// HTTP router shared state.
#[derive(Clone)]
pub struct HttpState {
    runtime: Arc<ServerRuntime>,
    handler: Arc<Mutex<CommandHandler>>,
    event_tx: broadcast::Sender<ClientNotification>,
}

/// Build an axum router for the HTTP/SSE API.
pub fn router(
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> Router {
    let handler = CommandHandler::new(Arc::clone(&runtime), event_tx.clone());
    let state = HttpState {
        runtime,
        handler: Arc::new(Mutex::new(handler)),
        event_tx,
    };

    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/:id/conversation", get(conversation_snapshot))
        .route("/api/sessions/:id/stream", get(session_stream))
        .route("/api/sessions/:id/prompt", post(submit_prompt))
        .route("/api/sessions/:id/compact", post(compact_session))
        .route("/api/sessions/:id/abort", post(abort_session))
        .route("/api/sessions/:id/fork", post(fork_session))
        .with_state(state)
}

async fn create_session(
    State(state): State<HttpState>,
    Json(request): Json<CreateSessionRequest>,
) -> Response {
    match state
        .handler
        .lock()
        .await
        .create_session(request.working_dir)
        .await
    {
        Ok(session_id) => Json(CreateSessionResponseDto { session_id }).into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "create_failed", error),
    }
}

async fn list_sessions(State(state): State<HttpState>) -> Response {
    match state.runtime.session_manager.list_summaries().await {
        Ok(summaries) => Json(SessionListResponseDto {
            sessions: summaries.into_iter().map(summary_to_dto).collect(),
        })
        .into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "list_failed", error),
    }
}

async fn conversation_snapshot(
    State(state): State<HttpState>,
    Path(session_id): Path<SessionId>,
) -> Response {
    match state
        .runtime
        .session_manager
        .conversation_snapshot(&session_id)
        .await
    {
        Ok(snapshot) => Json(conversation_to_dto(snapshot)).into_response(),
        Err(error) => error_response(StatusCode::NOT_FOUND, "session_not_found", error),
    }
}

async fn submit_prompt(
    State(state): State<HttpState>,
    Path(session_id): Path<SessionId>,
    Json(request): Json<PromptRequest>,
) -> Response {
    let result = state
        .handler
        .lock()
        .await
        .submit_prompt_for_session(session_id.clone(), request.text)
        .await;
    match result {
        Ok(turn_id) => Json(PromptSubmitResponse::Accepted {
            session_id,
            turn_id,
            branched_from_session_id: None,
        })
        .into_response(),
        Err(error) if error.contains("already running") => {
            error_response(StatusCode::CONFLICT, "turn_running", error)
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "prompt_failed", error),
    }
}

async fn compact_session(
    State(state): State<HttpState>,
    Path(session_id): Path<SessionId>,
    Json(_request): Json<CompactSessionRequest>,
) -> Response {
    match state
        .handler
        .lock()
        .await
        .compact_session(&session_id)
        .await
    {
        Ok(()) => Json(CompactSessionResponse {
            accepted: true,
            deferred: false,
            message: "compact accepted".into(),
        })
        .into_response(),
        Err(error) if error.contains("turn is running") => {
            error_response(StatusCode::CONFLICT, "turn_running", error)
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "compact_failed", error),
    }
}

async fn abort_session(
    State(state): State<HttpState>,
    Path(session_id): Path<SessionId>,
) -> Response {
    match state.handler.lock().await.abort_session(&session_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.contains("No active turn") => {
            error_response(StatusCode::NOT_FOUND, "no_active_turn", error)
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "abort_failed", error),
    }
}

async fn fork_session(
    Path(_session_id): Path<SessionId>,
    Json(_request): Json<ForkSessionRequest>,
) -> Response {
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        "fork_not_implemented",
        "fork is not implemented in HTTP v1",
    )
}

async fn session_stream(
    State(state): State<HttpState>,
    Path(session_id): Path<SessionId>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let runtime = Arc::clone(&state.runtime);
    let stream = stream::unfold(
        (rx, runtime, session_id, false),
        |(mut rx, runtime, session_id, closing)| async move {
            if closing {
                return None;
            }

            loop {
                match rx.recv().await {
                    Ok(ClientNotification::Event(event)) if event.session_id == session_id => {
                        let Some(delta) = event_to_delta(&event) else {
                            continue;
                        };
                        let cursor = event_cursor(&runtime, &event).await;
                        let item = sse_event(&ConversationStreamEnvelopeDto {
                            session_id: session_id.clone(),
                            cursor: ConversationCursorDto {
                                value: cursor.clone(),
                            },
                            delta,
                        })
                        .id(cursor);
                        return Some((Ok(item), (rx, runtime, session_id, false)));
                    },
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let cursor = state_cursor(&runtime, &session_id).await;
                        let item = sse_event(&ConversationStreamEnvelopeDto {
                            session_id: session_id.clone(),
                            cursor: ConversationCursorDto { value: cursor },
                            delta: ConversationDeltaDto::RehydrateRequired,
                        });
                        return Some((Ok(item), (rx, runtime, session_id, true)));
                    },
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn summary_to_dto(summary: SessionSummary) -> SessionListItemDto {
    let title = session_title(&summary.working_dir);
    SessionListItemDto {
        session_id: summary.session_id,
        working_dir: summary.working_dir,
        display_name: title.clone(),
        title,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        parent_session_id: summary.parent_session_id,
        parent_storage_seq: None,
        phase: summary.phase,
    }
}

fn conversation_to_dto(snapshot: ConversationReadModel) -> ConversationSnapshotResponseDto {
    let session = snapshot.session;
    let can_submit_prompt = matches!(session.phase, Phase::Idle | Phase::Error);
    ConversationSnapshotResponseDto {
        session_id: session.session_id.clone(),
        session_title: session_title(&session.working_dir),
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
        blocks: session
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| message_to_block(index, message))
            .collect(),
    }
}

fn event_to_delta(event: &Event) -> Option<ConversationDeltaDto> {
    match &event.payload {
        EventPayload::UserMessage { message_id, text } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::User {
                id: message_id.clone(),
                text: text.clone(),
            },
        }),
        EventPayload::AssistantMessageStarted { message_id } => {
            Some(ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.clone(),
                    text: String::new(),
                    status: ConversationBlockStatusDto::Streaming,
                },
            })
        },
        EventPayload::AssistantTextDelta { message_id, delta } => {
            Some(ConversationDeltaDto::PatchBlock {
                block_id: message_id.clone(),
                text_delta: delta.clone(),
            })
        },
        EventPayload::AssistantMessageCompleted { message_id, text } => {
            Some(ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.clone(),
                    text: text.clone(),
                    status: ConversationBlockStatusDto::Complete,
                },
            })
        },
        EventPayload::ToolCallStarted { call_id, tool_name } => {
            Some(ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::ToolCall {
                    id: call_id.clone(),
                    name: tool_name.clone(),
                    text: String::new(),
                    status: ConversationBlockStatusDto::Streaming,
                },
            })
        },
        EventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => Some(ConversationDeltaDto::ToolOutput {
            call_id: call_id.clone(),
            stream: *stream,
            delta: delta.clone(),
        }),
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::ToolCall {
                id: call_id.clone(),
                name: tool_name.clone(),
                text: result.content.clone(),
                status: if result.is_error {
                    ConversationBlockStatusDto::Error
                } else {
                    ConversationBlockStatusDto::Complete
                },
            },
        }),
        EventPayload::ErrorOccurred { message, .. } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::Error {
                id: event.id.clone(),
                message: message.clone(),
            },
        }),
        EventPayload::CompactionApplied { .. } => None,
        _ => Some(ConversationDeltaDto::UpdateControlState {
            control: control_from_phase(projected_phase(&event.payload)),
        }),
    }
}

fn projected_phase(payload: &EventPayload) -> Phase {
    match payload {
        EventPayload::TurnStarted
        | EventPayload::UserMessage { .. }
        | EventPayload::AgentRunStarted => Phase::Thinking,
        EventPayload::AssistantMessageStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. } => Phase::Streaming,
        EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolCallRequested { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::ToolCallCompleted { .. } => Phase::CallingTool,
        EventPayload::CompactionStarted => Phase::Compacting,
        EventPayload::ErrorOccurred { .. } => Phase::Error,
        _ => Phase::Idle,
    }
}

fn control_from_phase(phase: Phase) -> ConversationControlStateDto {
    let can_submit_prompt = matches!(phase, Phase::Idle | Phase::Error);
    ConversationControlStateDto {
        phase,
        can_submit_prompt,
        can_request_compact: can_submit_prompt,
        compact_pending: false,
        compacting: matches!(phase, Phase::Compacting),
        current_mode_id: None,
        active_turn_id: None,
    }
}

fn message_to_block(index: usize, message: &LlmMessage) -> ConversationBlockDto {
    let id = format!("snapshot-message-{index}");
    let text = message
        .content
        .iter()
        .map(content_to_text)
        .collect::<Vec<_>>()
        .join("");
    match message.role {
        LlmRole::User => ConversationBlockDto::User { id, text },
        LlmRole::Assistant => ConversationBlockDto::Assistant {
            id,
            text,
            status: ConversationBlockStatusDto::Complete,
        },
        LlmRole::Tool => ConversationBlockDto::ToolCall {
            id,
            name: message.name.clone().unwrap_or_else(|| "tool".into()),
            text,
            status: ConversationBlockStatusDto::Complete,
        },
        LlmRole::System => ConversationBlockDto::SystemNote { id, text },
    }
}

fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => format!("tool call: {name}({arguments})"),
        LlmContent::ToolResult { content, .. } => content.clone(),
    }
}

async fn event_cursor(runtime: &ServerRuntime, event: &Event) -> String {
    if let Some(seq) = event.seq {
        seq.to_string()
    } else {
        state_cursor(runtime, &event.session_id).await
    }
}

async fn state_cursor(runtime: &ServerRuntime, session_id: &SessionId) -> String {
    runtime
        .session_manager
        .latest_cursor(session_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "0".into())
}

fn sse_event<T: serde::Serialize>(value: &T) -> SseEvent {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    SseEvent::default().event("conversation").data(data)
}

fn error_response(status: StatusCode, code: impl Into<String>, message: impl ToString) -> Response {
    (
        status,
        Json(ConversationErrorEnvelopeDto {
            code: code.into(),
            message: message.to_string(),
        }),
    )
        .into_response()
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
    use super::*;

    #[test]
    fn fork_request_shape_deserializes() {
        let request: ForkSessionRequest =
            serde_json::from_value(serde_json::json!({"turnId":"t1","storageSeq":7})).unwrap();
        assert_eq!(request.turn_id.as_deref(), Some("t1"));
        assert_eq!(request.storage_seq, Some(7));
    }

    #[test]
    fn conversation_snapshot_cursor_is_full_snapshot_version() {
        let mut session = astrcode_core::storage::SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(9);
        session.messages.push(LlmMessage::user("hello"));

        let dto = conversation_to_dto(ConversationReadModel { session });

        assert_eq!(dto.cursor.value, "9");
        assert_eq!(dto.blocks.len(), 1);
    }
}
