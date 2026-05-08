//! Axum HTTP/SSE 入口。
//!
//! 这层只做 wire 适配：命令统一进入 [`CommandHandler`]，读接口从 storage
//! read model 映射到 `astrcode_protocol::http` DTO。

use std::{convert::Infallible, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmMessage, LlmRole},
    storage::{SessionReadModel, SessionSummary},
    types::SessionId,
};
use astrcode_protocol::{
    commands::ClientCommand,
    events::ClientNotification,
    http::{
        AvailableModelDto, CompactSessionRequest, CompactSessionResponse, ConfigReloadResponseDto,
        ConfigViewResponseDto, ConversationBlockDto, ConversationBlockStatusDto,
        ConversationControlStateDto, ConversationCursorDto, ConversationDeltaDto,
        ConversationErrorEnvelopeDto, ConversationSnapshotResponseDto,
        ConversationStreamEnvelopeDto, CreateSessionRequest, CreateSessionResponseDto,
        CurrentModelResponseDto, DeleteProjectResponseDto, ModelDto, ModelListResponseDto,
        ModelTestResponseDto, ProfileDto, PromptRequest, PromptSubmitResponse, SessionListItemDto,
        SessionListResponseDto, SlashCommandInfoDto, SlashCommandListResponseDto,
        UpdateActiveSelectionRequest, UpdateActiveSelectionResponseDto,
    },
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use crate::{
    bootstrap::ServerRuntime,
    handler::{CommandHandler, ManualCompactOutcome, PromptSubmission},
};

/// HTTP router shared state.
#[derive(Clone)]
pub struct HttpState {
    runtime: Arc<ServerRuntime>,
    handler: crate::handler::CommandHandle,
    event_tx: broadcast::Sender<ClientNotification>,
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteProjectParams {
    working_dir: String,
}

/// Run the HTTP/SSE server until graceful shutdown.
pub async fn run_http_server(
    runtime: Arc<ServerRuntime>,
    addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (event_tx, _) = broadcast::channel(256);
    let shutdown_token = runtime.shutdown_token.clone();
    let app = router(Arc::clone(&runtime), event_tx);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("HTTP server ready at http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("graceful shutdown triggered");
        })
        .await?;
    Ok(())
}

/// Build an axum router for the HTTP/SSE API.
pub fn router(
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> Router {
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx.clone());
    let state = HttpState {
        runtime,
        handler,
        event_tx,
    };

    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/sessions/{id}/conversation",
            get(conversation_snapshot),
        )
        .route("/api/sessions/{id}/stream", get(session_stream))
        .route("/api/sessions/{id}/prompt", post(submit_prompt))
        .route("/api/sessions/{id}/commands", get(list_commands))
        .route("/api/sessions/{id}/compact", post(compact_session))
        .route("/api/sessions/{id}/abort", post(abort_session))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/projects", delete(delete_project))
        .route("/api/config", get(get_config))
        .route("/api/config/reload", post(reload_config))
        .route(
            "/api/config/active-selection",
            post(update_active_selection),
        )
        .route("/api/models/current", get(get_current_model))
        .route("/api/models", get(list_models))
        .route("/api/models/test", post(test_model))
        .route("/api/shutdown", post(shutdown))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn create_session(
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
    Path(session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state
        .runtime
        .session_manager
        .read_model(&session_id)
        .await
    {
        Ok(snapshot) => Json(conversation_to_dto(snapshot)).into_response(),
        Err(error) => error_response(StatusCode::NOT_FOUND, "session_not_found", error),
    }
}

async fn submit_prompt(
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
        Err(error) if error.contains("already running") => {
            tracing::warn!(session_id = %session_id, "prompt rejected: turn already running");
            error_response(StatusCode::CONFLICT, "turn_running", error)
        },
        Err(error) if error.contains("Unknown command") => {
            tracing::warn!(session_id = %session_id, error = %error, "prompt rejected: unknown slash command");
            error_response(StatusCode::BAD_REQUEST, "unknown_command", error)
        },
        Err(error) => {
            tracing::error!(session_id = %session_id, error = %error, "prompt failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "prompt_failed", error)
        },
    }
}

async fn list_commands(State(state): State<HttpState>, Path(session_id): Path<String>) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.command_infos_for_session(session_id).await {
        Ok(commands) => Json(SlashCommandListResponseDto {
            commands: commands
                .into_iter()
                .map(|command| SlashCommandInfoDto {
                    name: command.name,
                    description: command.description,
                    needs_argument: command.needs_argument,
                    source: command.source,
                })
                .collect(),
        })
        .into_response(),
        Err(error) => error_response(StatusCode::NOT_FOUND, "session_not_found", error),
    }
}

async fn compact_session(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Json(_request): Json<CompactSessionRequest>,
) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.compact_session(session_id).await {
        Ok(ManualCompactOutcome::Created { child_session_id }) => Json(CompactSessionResponse {
            accepted: true,
            deferred: false,
            new_session_id: Some(child_session_id.into_string()),
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
        Err(error) if error.contains("turn is running") => {
            error_response(StatusCode::CONFLICT, "turn_running", error)
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "compact_failed", error),
    }
}

async fn abort_session(State(state): State<HttpState>, Path(session_id): Path<String>) -> Response {
    let session_id = SessionId::from(session_id);
    match state.handler.abort_session(session_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.contains("No active turn") => {
            error_response(StatusCode::NOT_FOUND, "no_active_turn", error)
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "abort_failed", error),
    }
}

async fn delete_session(
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

async fn delete_project(
    State(state): State<HttpState>,
    Query(params): Query<DeleteProjectParams>,
) -> Response {
    match state.runtime.session_manager.list_summaries().await {
        Ok(summaries) => {
            let matching: Vec<_> = summaries
                .into_iter()
                .filter(|s| s.working_dir == params.working_dir)
                .collect();
            let count = matching.len();
            for summary in &matching {
                if let Err(error) = state
                    .handler
                    .handle(ClientCommand::DeleteSession {
                        session_id: summary.session_id.to_string(),
                    })
                    .await
                {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "delete_project_failed",
                        error,
                    );
                }
            }
            Json(DeleteProjectResponseDto {
                deleted_count: count,
            })
            .into_response()
        },
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "list_failed", error),
    }
}

async fn get_config(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.read_raw_config();
    let config_path = state.runtime.config_store.path().display().to_string();
    let profiles: Vec<ProfileDto> = raw
        .profiles
        .iter()
        .map(|p| ProfileDto {
            name: p.name.clone(),
            provider_kind: p.provider_kind.clone(),
            base_url: p.base_url.clone(),
            has_api_key: p.api_key.as_ref().is_some_and(|k| !k.is_empty()),
            models: p
                .models
                .iter()
                .map(|m| ModelDto {
                    id: m.id.clone(),
                    max_tokens: m.max_tokens,
                    context_limit: m.context_limit,
                })
                .collect(),
        })
        .collect();
    Json(ConfigViewResponseDto {
        config_path,
        active_profile: raw.active_profile.clone(),
        active_model: raw.active_model.clone(),
        profiles,
        warning: None,
    })
    .into_response()
}

async fn reload_config(State(state): State<HttpState>) -> Response {
    match state.runtime.config_store.load().await {
        Ok(config) => {
            let active_profile = config.active_profile.clone();
            let active_model = config.active_model.clone();
            {
                let mut guard = state.runtime.write_raw_config();
                *guard = config;
            }
            let _ = state.runtime.sync_effective();
            Json(ConfigReloadResponseDto {
                active_profile,
                active_model,
            })
            .into_response()
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reload_failed",
            error.to_string(),
        ),
    }
}

async fn update_active_selection(
    State(state): State<HttpState>,
    Json(request): Json<UpdateActiveSelectionRequest>,
) -> Response {
    let updated = {
        let mut guard = state.runtime.write_raw_config();
        guard.active_profile = request.active_profile;
        guard.active_model = request.active_model;
        guard.clone()
    };
    match state.runtime.config_store.save(&updated).await {
        Ok(()) => {
            let warning = state.runtime.sync_effective().err().map(|e| e.to_string());
            Json(UpdateActiveSelectionResponseDto {
                success: true,
                warning,
            })
            .into_response()
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            error.to_string(),
        ),
    }
}

async fn get_current_model(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.read_raw_config();
    let eff = state.runtime.read_effective();
    Json(CurrentModelResponseDto {
        profile_name: raw.active_profile.clone(),
        model_id: eff.llm.model_id.clone(),
        provider_kind: eff.llm.provider_kind.clone(),
    })
    .into_response()
}

async fn list_models(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.read_raw_config();
    let models: Vec<AvailableModelDto> = raw
        .profiles
        .iter()
        .flat_map(|p| {
            p.models.iter().map(|m| AvailableModelDto {
                profile_name: p.name.clone(),
                model_id: m.id.clone(),
                provider_kind: p.provider_kind.clone(),
            })
        })
        .collect();
    Json(ModelListResponseDto { models }).into_response()
}

async fn test_model(State(state): State<HttpState>) -> Response {
    let start = std::time::Instant::now();
    match state
        .runtime
        .llm_provider
        .generate(vec![astrcode_core::llm::LlmMessage::user("Hi")], vec![])
        .await
    {
        Ok(mut rx) => {
            while rx.recv().await.is_some() {}
            Json(ModelTestResponseDto {
                success: true,
                message: format!("ok ({}ms)", start.elapsed().as_millis()),
            })
            .into_response()
        },
        Err(error) => Json(ModelTestResponseDto {
            success: false,
            message: error.to_string(),
        })
        .into_response(),
    }
}

async fn session_stream(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Query(query): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    tracing::info!(session_id = %session_id, cursor = ?query.cursor, "SSE stream connected");
    let session_id = SessionId::from(session_id);
    let rx = state.event_tx.subscribe();
    let (missed_events, replay_error) = match query.cursor.as_ref() {
        Some(cursor) if cursor.parse::<u64>().is_err() => (Vec::new(), true),
        Some(cursor) => match state
            .runtime
            .session_manager
            .replay_after(&session_id, cursor)
            .await
        {
            Ok(events) => (events, false),
            Err(error) => {
                tracing::warn!(session_id = %session_id, cursor, "failed to replay SSE cursor: {error}");
                (Vec::new(), true)
            },
        },
        None => (Vec::new(), false),
    };
    let replay_max_seq = missed_events.iter().filter_map(|event| event.seq).max();
    let replay_runtime = Arc::clone(&state.runtime);
    let replay_session_id = session_id.clone();
    let replay_stream = stream::iter(missed_events).filter_map(move |event| {
        let runtime = Arc::clone(&replay_runtime);
        let session_id = replay_session_id.clone();
        async move {
            let delta = event_to_replay_delta(&event)?;
            let cursor = event_cursor(&runtime, &event).await;
            Some(Ok(sse_event(&ConversationStreamEnvelopeDto {
                session_id: session_id.to_string(),
                cursor: ConversationCursorDto {
                    value: cursor.clone(),
                },
                delta,
            })
            .id(cursor)))
        }
    });
    let replay_error_stream = stream::iter(replay_error.then(|| {
        Ok(sse_event(&ConversationStreamEnvelopeDto {
            session_id: session_id.to_string(),
            cursor: ConversationCursorDto { value: "0".into() },
            delta: ConversationDeltaDto::RehydrateRequired,
        }))
    }));

    let runtime = Arc::clone(&state.runtime);
    let live_stream = stream::unfold(
        (rx, runtime, session_id, replay_max_seq, false, std::collections::VecDeque::<Result<axum::response::sse::Event, std::convert::Infallible>>::new()),
        |(mut rx, runtime, session_id, replay_max_seq, closing, mut pending)| async move {
            if closing {
                return None;
            }

            if let Some(item) = pending.pop_front() {
                return Some((item, (rx, runtime, session_id, replay_max_seq, false, pending)));
            }

            loop {
                match rx.recv().await {
                    Ok(ClientNotification::Event(event)) if event.session_id == session_id => {
                        if replay_max_seq
                            .zip(event.seq)
                            .is_some_and(|(max_seq, event_seq)| event_seq <= max_seq)
                        {
                            continue;
                        }
                        let deltas = event_to_deltas(&event);
                        if deltas.is_empty() {
                            continue;
                        }
                        let cursor = event_cursor(&runtime, &event).await;
                        let items: std::collections::VecDeque<_> = deltas
                            .into_iter()
                            .map(|delta| {
                                Ok(sse_event(&ConversationStreamEnvelopeDto {
                                    session_id: session_id.to_string(),
                                    cursor: ConversationCursorDto {
                                        value: cursor.clone(),
                                    },
                                    delta,
                                })
                                .id(cursor.clone()))
                            })
                            .collect();
                        let mut items = items;
                        let first = items.pop_front().unwrap();
                        return Some((first, (rx, runtime, session_id, replay_max_seq, false, items)));
                    },
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let cursor = state_cursor(&runtime, &session_id).await;
                        let item = Ok(sse_event(&ConversationStreamEnvelopeDto {
                            session_id: session_id.to_string(),
                            cursor: ConversationCursorDto { value: cursor },
                            delta: ConversationDeltaDto::RehydrateRequired,
                        }));
                        return Some((item, (rx, runtime, session_id, replay_max_seq, true, pending)));
                    },
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    let stream = replay_error_stream.chain(replay_stream).chain(live_stream);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn summary_to_dto(summary: SessionSummary) -> SessionListItemDto {
    let title = session_title(&summary.working_dir);
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
    }
}

fn conversation_to_dto(session: SessionReadModel) -> ConversationSnapshotResponseDto {
    let can_submit_prompt = matches!(session.phase, Phase::Idle | Phase::Error);
    ConversationSnapshotResponseDto {
        session_id: session.session_id.to_string(),
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

fn event_to_deltas(event: &Event) -> Vec<ConversationDeltaDto> {
    match &event.payload {
        EventPayload::UserMessage { message_id, text } => vec![ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::User {
                id: message_id.to_string(),
                text: text.clone(),
            },
        }],
        EventPayload::AssistantMessageStarted { message_id } => {
            vec![ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.to_string(),
                    text: String::new(),
                    status: ConversationBlockStatusDto::Streaming,
                },
            }]
        },
        EventPayload::AssistantTextDelta { message_id, delta } => {
            vec![ConversationDeltaDto::PatchBlock {
                block_id: message_id.to_string(),
                text_delta: delta.clone(),
            }]
        },
        EventPayload::AssistantMessageCompleted { message_id, text } => {
            vec![ConversationDeltaDto::FinalizeBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.to_string(),
                    text: text.clone(),
                    status: ConversationBlockStatusDto::Complete,
                },
            }]
        },
        EventPayload::ToolCallStarted { call_id, tool_name } => {
            vec![ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::ToolCall {
                    id: call_id.to_string(),
                    name: tool_name.clone(),
                    text: String::new(),
                    status: ConversationBlockStatusDto::Streaming,
                },
            }]
        },
        EventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => vec![ConversationDeltaDto::ToolOutput {
            call_id: call_id.to_string(),
            stream: *stream,
            delta: delta.clone(),
        }],
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => vec![ConversationDeltaDto::FinalizeBlock {
            block: ConversationBlockDto::ToolCall {
                id: call_id.to_string(),
                name: tool_name.clone(),
                text: result.content.clone(),
                status: if result.is_error {
                    ConversationBlockStatusDto::Error
                } else {
                    ConversationBlockStatusDto::Complete
                },
            },
        }],
        EventPayload::ErrorOccurred { message, .. } => vec![ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::Error {
                id: event.id.to_string(),
                message: message.clone(),
            },
        }],
        EventPayload::CompactBoundaryCreated {
            trigger,
            pre_tokens,
            post_tokens,
            summary,
            transcript_path,
            continued_session_id,
        } => {
            let block_id = format!("compact-{}", event.seq.unwrap_or_default());
            vec![
                ConversationDeltaDto::AppendBlock {
                    block: ConversationBlockDto::CompactSummary {
                        id: block_id,
                        summary: summary.clone(),
                        trigger: trigger.clone(),
                        pre_tokens: *pre_tokens,
                        post_tokens: *post_tokens,
                        transcript_path: transcript_path.clone(),
                    },
                },
                ConversationDeltaDto::SessionContinued {
                    parent_session_id: event.session_id.to_string(),
                    new_session_id: continued_session_id.to_string(),
                    parent_cursor: ConversationCursorDto {
                        value: event.seq.unwrap_or_default().to_string(),
                    },
                },
            ]
        },
        // Phase transitions that the client needs to know about
        EventPayload::TurnStarted
        | EventPayload::AgentRunStarted
        | EventPayload::CompactionStarted
        | EventPayload::ToolCallBackgrounded { .. }
        | EventPayload::BackgroundTaskCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(projected_phase(&event.payload)),
            }]
        },
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(Phase::Idle),
            }]
        },
        EventPayload::ThinkingDelta { delta } => vec![ConversationDeltaDto::ThinkingDelta {
            delta: delta.clone(),
        }],
        // Terminal events where the client already has the block content
        EventPayload::SystemPromptConfigured { .. }
        | EventPayload::SessionContinuedFromCompaction { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolCallRequested { .. } => vec![],
        _ => vec![],
    }
}

fn event_to_replay_delta(event: &Event) -> Option<ConversationDeltaDto> {
    match &event.payload {
        EventPayload::UserMessage { message_id, text } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::User {
                id: message_id.to_string(),
                text: text.clone(),
            },
        }),
        EventPayload::AssistantMessageCompleted { message_id, text } => {
            Some(ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.to_string(),
                    text: text.clone(),
                    status: ConversationBlockStatusDto::Complete,
                },
            })
        },
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::ToolCall {
                id: call_id.to_string(),
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
                id: event.id.to_string(),
                message: message.clone(),
            },
        }),
        EventPayload::CompactBoundaryCreated {
            trigger,
            pre_tokens,
            post_tokens,
            summary,
            transcript_path,
            ..
        } => Some(ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::CompactSummary {
                id: format!("compact-{}", event.seq.unwrap_or_default()),
                summary: summary.clone(),
                trigger: trigger.clone(),
                pre_tokens: *pre_tokens,
                post_tokens: *post_tokens,
                transcript_path: transcript_path.clone(),
            },
        }),
        EventPayload::TurnCompleted { .. } => Some(ConversationDeltaDto::UpdateControlState {
            control: control_from_phase(Phase::Idle),
        }),
        EventPayload::SessionContinuedFromCompaction { .. }
        | EventPayload::SessionStarted { .. }
        | EventPayload::SystemPromptConfigured { .. }
        | EventPayload::TurnStarted
        | EventPayload::AgentRunStarted
        | EventPayload::AgentRunCompleted { .. }
        | EventPayload::AssistantMessageStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. }
        | EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolCallRequested { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::CompactionStarted
        | EventPayload::ToolCallBackgrounded { .. }
        | EventPayload::BackgroundTaskOutput { .. }
        | EventPayload::BackgroundTaskCompleted { .. }
        | EventPayload::Custom { .. }
        | EventPayload::SessionDeleted => None,
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
        | EventPayload::ToolCallCompleted { .. }
        | EventPayload::ToolCallBackgrounded { .. } => Phase::CallingTool,
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
        .map(crate::handler::snapshot::content_to_text)
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

async fn shutdown(State(state): State<HttpState>) -> Response {
    tracing::info!("shutdown requested via HTTP");
    let runtime = Arc::clone(&state.runtime);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        runtime.shutdown_token.cancel();
    });
    StatusCode::NO_CONTENT.into_response()
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
    fn conversation_snapshot_cursor_is_full_snapshot_version() {
        let mut session = astrcode_core::storage::SessionReadModel::empty("session-1".into());
        session.working_dir = "D:/work/project".into();
        session.latest_seq = Some(9);
        session.messages.push(LlmMessage::user("hello"));

        let dto = conversation_to_dto(session);

        assert_eq!(dto.cursor.value, "9");
        assert_eq!(dto.blocks.len(), 1);
    }

    #[test]
    fn assistant_completion_finalizes_with_full_text() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: "assistant-1".into(),
                text: "complete answer".into(),
            },
        );

        let deltas = event_to_deltas(&event);
        assert_eq!(deltas.len(), 1, "assistant completion should produce one delta");
        let delta = deltas.into_iter().next().unwrap();

        match delta {
            ConversationDeltaDto::FinalizeBlock {
                block: ConversationBlockDto::Assistant { id, text, status },
            } => {
                assert_eq!(id, "assistant-1");
                assert_eq!(text, "complete answer");
                assert!(matches!(status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn tool_completion_finalizes_with_result_content() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::ToolCallCompleted {
                call_id: "tool-1".into(),
                tool_name: "read".into(),
                result: astrcode_core::tool::ToolResult {
                    call_id: "tool-1".into(),
                    content: "file contents".into(),
                    is_error: false,
                    error: None,
                    metadata: Default::default(),
                    duration_ms: None,
                },
            },
        );

        let deltas = event_to_deltas(&event);
        assert_eq!(deltas.len(), 1, "tool completion should produce one delta");
        let delta = deltas.into_iter().next().unwrap();

        match delta {
            ConversationDeltaDto::FinalizeBlock {
                block:
                    ConversationBlockDto::ToolCall {
                        id,
                        name,
                        text,
                        status,
                    },
            } => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "read");
                assert_eq!(text, "file contents");
                assert!(matches!(status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }
}
