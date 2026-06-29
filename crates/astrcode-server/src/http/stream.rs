//! `GET /api/sessions/{id}/stream` SSE 流。
//!
//! 三段流串联：
//! 1. `replay_error_stream`：cursor 解析或重放失败时推一条 `RehydrateRequired`。
//! 2. `replay_stream`：从 `EventStore` 拉历史事件转 deltas（按 cursor 起点）。
//! 3. `live_stream`：订阅当前 conversation 的 scoped event fanout 与全局非事件通知。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    storage::AgentSessionStatus,
    types::{Cursor, SessionId},
};
use astrcode_protocol::{
    agent_session_link::AgentSessionLinkDto,
    events::ClientNotification,
    http::{
        ConversationBlockDto, ConversationCursorDto, ConversationDeltaDto,
        ConversationStreamEnvelopeDto,
    },
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    HttpState, error_response,
    projection::{live::event_to_deltas, replay::event_to_replay_deltas},
};
use crate::bootstrap::ServerRuntime;

type SseItem = Result<axum::response::sse::Event, std::convert::Infallible>;

/// SSE live stream 的内部状态。
///
/// 从 `stream::unfold` 的匿名元组中抽出，提高可读性并方便未来扩展。
struct LiveStreamState {
    event_rx: mpsc::Receiver<Arc<Event>>,
    notification_rx: mpsc::Receiver<ClientNotification>,
    runtime: Arc<ServerRuntime>,
    session_id: SessionId,
    /// replay 阶段已发送的最大 seq，live 阶段跳过 <= 该值的事件避免重复。
    replay_max_seq: Option<u64>,
    /// 需要主动关闭流时设为 true，下一次 poll 返回 None。
    closing: bool,
    /// 单个事件产出多条 delta 时，剩余待发送的缓冲。
    pending: std::collections::VecDeque<SseItem>,
    /// 会话是否已有消息，用于正确计算 can_request_compact。
    has_messages: bool,
    /// 是否已完成初始 stale event 排水。
    /// replay_max_seq 存在时为 false，首次 unfold 调用时执行一次性排水。
    drained: bool,
    /// 父会话中的 initial child id -> 当前 leaf child id。
    child_sessions: HashMap<SessionId, SessionId>,
    /// 当前 leaf child id -> initial child id，用于 O(1) 匹配子会话 live 事件。
    leaf_child_sessions: HashMap<SessionId, SessionId>,
    /// 子会话最近 live 阶段，用于避免重复投影。
    last_child_phase: HashMap<SessionId, Phase>,
    /// 缓存的最新 cursor，用于 live-only 事件（避免每次查询存储）。
    cached_cursor: Option<String>,
}

enum LiveInput {
    Event(Arc<Event>),
    Notification(Box<ClientNotification>),
}

#[derive(Debug, Deserialize)]
pub(super) struct StreamQuery {
    cursor: Option<String>,
}

pub(in crate::http) async fn session_stream(
    State(http_state): State<HttpState>,
    Path(raw_session_id): Path<String>,
    Query(query): Query<StreamQuery>,
) -> Response {
    tracing::info!(session_id = %raw_session_id, cursor = ?query.cursor, "SSE stream connected");
    let session_id = SessionId::from(raw_session_id);

    // Validate session exists before opening the stream.
    let has_messages = match http_state
        .runtime
        .session_manager
        .has_messages(&session_id)
        .await
    {
        Ok(has_messages) => has_messages,
        Err(_) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "Session not found",
            );
        },
    };
    let agent_sessions = match http_state
        .runtime
        .session_manager
        .agent_sessions(&session_id)
        .await
    {
        Ok(agent_sessions) => agent_sessions,
        Err(error) => {
            tracing::warn!(session_id = %session_id, "failed to read agent sessions for SSE stream: {error}");
            return error_response(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "Session not found",
            );
        },
    };
    let child_sessions = agent_sessions
        .iter()
        .filter(|link| link.status == AgentSessionStatus::Running)
        .map(|link| {
            (
                link.child_session_id.clone(),
                link.final_session_id
                    .clone()
                    .unwrap_or_else(|| link.child_session_id.clone()),
            )
        })
        .collect();
    let leaf_child_sessions = reverse_child_session_index(&child_sessions);
    let last_child_phase = agent_sessions
        .iter()
        .filter_map(|link| {
            link.phase
                .map(|phase| (link.child_session_id.clone(), phase))
        })
        .collect();

    http_state
        .event_bus
        .register_conversation_children(&session_id, &child_sessions);
    let event_rx = http_state
        .event_bus
        .subscribe_conversation_events(&session_id);
    let notification_rx = http_state.event_bus.subscribe_global_notifications();
    let (missed_events, replay_error) = match query.cursor.as_ref() {
        Some(cursor) if cursor.parse::<u64>().is_err() => (Vec::new(), true),
        Some(cursor) => match http_state
            .runtime
            .session_manager
            .replay_from(&session_id, &Cursor::from(cursor.as_str()))
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
    let replay_runtime = Arc::clone(&http_state.runtime);
    let replay_event_bus = Arc::clone(&http_state.event_bus);
    let replay_session_id = session_id.clone();
    let replay_has_messages = has_messages;
    let replay_stream = stream::iter(missed_events)
        .then(move |event| {
            let runtime = Arc::clone(&replay_runtime);
            let event_bus = Arc::clone(&replay_event_bus);
            let replay_sid = replay_session_id.clone();
            async move {
                let mut deltas = event_to_replay_deltas(&event, replay_has_messages);
                // 如果重放 AssistantMessageStarted 且该消息仍在流式传输，
                // 补一个 PatchBlock 让客户端拿到已积累的文本。
                if let EventPayload::AssistantMessageStarted { message_id } = &event.payload {
                    if let Some(msg) = event_bus.streaming_snapshot(&replay_sid) {
                        if msg.message_id == message_id.as_str() {
                            if !msg.text.is_empty() {
                                deltas.push(ConversationDeltaDto::PatchBlock {
                                    block_id: message_id.to_string(),
                                    text_delta: msg.text,
                                });
                            }
                            if let Some(reasoning) = msg.reasoning_content {
                                if !reasoning.is_empty() {
                                    deltas.push(ConversationDeltaDto::ThinkingDelta {
                                        block_id: message_id.to_string(),
                                        delta: reasoning,
                                    });
                                }
                            }
                        }
                    }
                }
                let cursor = event_cursor(&runtime, &event).await;
                deltas
                    .into_iter()
                    .map(|delta| {
                        Ok(sse_event(&ConversationStreamEnvelopeDto {
                            session_id: replay_sid.to_string(),
                            cursor: ConversationCursorDto {
                                value: cursor.clone(),
                            },
                            delta,
                        }))
                    })
                    .collect::<Vec<_>>()
            }
        })
        .flat_map(stream::iter);
    let replay_error_stream = stream::iter(replay_error.then(|| {
        Ok(sse_event(&ConversationStreamEnvelopeDto {
            session_id: session_id.to_string(),
            cursor: ConversationCursorDto { value: "0".into() },
            delta: ConversationDeltaDto::RehydrateRequired,
        }))
    }));

    let live_runtime = Arc::clone(&http_state.runtime);
    let live_stream = stream::unfold(
        LiveStreamState {
            event_rx,
            notification_rx,
            runtime: live_runtime,
            session_id,
            replay_max_seq,
            closing: false,
            pending: std::collections::VecDeque::new(),
            has_messages,
            drained: false,
            cached_cursor: None,
            child_sessions,
            leaf_child_sessions,
            last_child_phase,
        },
        |mut state| async move {
            if state.closing {
                return None;
            }

            // 首次进入 live 阶段时，一次性排空 rx 缓冲区中的 stale 事件。
            // replay 阶段已通过 durable event 重建了完整状态；缓冲区中的 live-only
            // 事件（AssistantTextDelta / ToolOutputDelta 等，无 seq）属于 replay
            // 覆盖时段的残留，送达后会导致前端对已 finalize 的 block 重复追加。
            // 排水仅丢弃 live-only 事件；seq > replay_max_seq 的 durable 事件保留。
            if !state.drained {
                state.drained = true;
                drain_stale_live_events(&mut state).await;
            }

            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }

            loop {
                match recv_live_input(&mut state).await {
                    Some(input) => {
                        let mut items: std::collections::VecDeque<_> =
                            live_input_to_sse_items(&mut state, input).await.into();
                        // Non-blocking drain: if more notifications are already
                        // buffered in the channel, process them now so they are
                        // sent in the same HTTP chunk as the first one.
                        drain_pending_live_inputs(&mut state, &mut items).await;

                        if items.is_empty() {
                            continue;
                        }
                        let Some(first) = items.pop_front() else {
                            continue;
                        };
                        state.pending = items;
                        return Some((first, state));
                    },
                    None => return None,
                }
            }
        },
    );
    let stream = replay_error_stream.chain(replay_stream).chain(live_stream);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn recv_live_input(state: &mut LiveStreamState) -> Option<LiveInput> {
    // Conversation events and global notifications are intentionally separate
    // channels. We preserve ordering inside each channel, but do not promise a
    // total order across them.
    tokio::select! {
        event = state.event_rx.recv() => event.map(LiveInput::Event),
        notification = state.notification_rx.recv() => {
            notification.map(|notification| LiveInput::Notification(Box::new(notification)))
        },
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
    match runtime.session_manager().latest_cursor(session_id).await {
        Ok(Some(cursor)) => cursor,
        Ok(None) => "0".to_string(),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to read latest cursor from storage, falling back to 0"
            );
            "0".to_string()
        },
    }
}

/// Non-blocking drain: if more live inputs are already buffered in the
/// channel, process them now so they are batched into the same HTTP chunk.
async fn drain_pending_live_inputs(
    state: &mut LiveStreamState,
    items: &mut std::collections::VecDeque<SseItem>,
) {
    loop {
        let mut drained = false;
        while let Ok(event) = state.event_rx.try_recv() {
            drained = true;
            let more = live_input_to_sse_items(state, LiveInput::Event(event)).await;
            items.extend(more);
        }
        while let Ok(notification) = state.notification_rx.try_recv() {
            drained = true;
            let more =
                live_input_to_sse_items(state, LiveInput::Notification(Box::new(notification)))
                    .await;
            items.extend(more);
        }
        if !drained {
            break;
        }
    }
}

/// 一次性排空 rx 缓冲区，丢弃 replay 时段残留的 live-only 事件。
///
/// 仅在有 replay_max_seq 时有效（即客户端携带 cursor 连接、有历史事件重放）。
/// durable 事件（带 seq）且 seq > replay_max_seq 的保留到 pending。
async fn drain_stale_live_events(state: &mut LiveStreamState) {
    let Some(replay_max) = state.replay_max_seq else {
        return;
    };
    let mut buffered = Vec::new();
    while let Ok(event) = state.event_rx.try_recv() {
        if event.session_id == state.session_id {
            // live-only 事件（无 seq）属于 replay 时段残留，直接丢弃。
            let Some(seq) = event.seq else {
                continue;
            };
            // 已被 replay 覆盖的 durable 事件也丢弃。
            if seq <= replay_max {
                continue;
            }
            buffered.push(LiveInput::Event(event));
        } else if is_tracked_child_event(state, &event) {
            buffered.push(LiveInput::Event(event));
        }
    }
    while let Ok(notification) = state.notification_rx.try_recv() {
        match notification {
            ClientNotification::StatusItemUpdate { .. }
            | ClientNotification::ExtensionRegistryChanged
            | ClientNotification::ExtensionCommandResult { .. } => {
                buffered.push(LiveInput::Notification(Box::new(notification)));
            },
            ClientNotification::Event(_) => {},
            _ => {},
        }
    }
    for input in buffered {
        let items = live_input_to_sse_items(state, input).await;
        state.pending.extend(items);
    }
}

async fn live_input_to_sse_items(state: &mut LiveStreamState, input: LiveInput) -> Vec<SseItem> {
    match input {
        LiveInput::Event(event) => event_to_sse_items(state, event).await,
        LiveInput::Notification(notification) => {
            notification_to_sse_items(state, *notification).await
        },
    }
}

async fn event_to_sse_items(state: &mut LiveStreamState, event: Arc<Event>) -> Vec<SseItem> {
    match event.as_ref() {
        event if event.session_id == state.session_id => {
            if state
                .replay_max_seq
                .zip(event.seq)
                .is_some_and(|(max_seq, event_seq)| event_seq <= max_seq)
            {
                return Vec::new();
            }
            if event_adds_message(event) {
                state.has_messages = true;
            }
            update_child_tracking_from_parent_event(state, event);

            // 更新缓存的 cursor（如果有 seq）
            if let Some(seq) = event.seq {
                state.cached_cursor = Some(seq.to_string());
            }

            let deltas = event_to_deltas(event, state.has_messages);
            if deltas.is_empty() {
                return Vec::new();
            }

            // 使用缓存的 cursor（live-only 事件）或事件的 seq
            let cursor = if let Some(seq) = event.seq {
                seq.to_string()
            } else {
                get_or_fetch_cursor(state).await
            };

            deltas
                .into_iter()
                .map(|delta| {
                    Ok(sse_event(&ConversationStreamEnvelopeDto {
                        session_id: state.session_id.to_string(),
                        cursor: ConversationCursorDto {
                            value: cursor.clone(),
                        },
                        delta,
                    }))
                })
                .collect()
        },
        event => {
            let Some(delta) = child_event_to_agent_update(state, event) else {
                return Vec::new();
            };
            // 子事件的 seq 属于子会话，不能用来更新父会话的 cursor
            let cursor = get_or_fetch_cursor(state).await;
            vec![Ok(sse_event(&ConversationStreamEnvelopeDto {
                session_id: state.session_id.to_string(),
                cursor: ConversationCursorDto { value: cursor },
                delta,
            }))]
        },
    }
}

async fn notification_to_sse_items(
    state: &mut LiveStreamState,
    notification: ClientNotification,
) -> Vec<SseItem> {
    match notification {
        ClientNotification::Event(_) => Vec::new(),
        ClientNotification::StatusItemUpdate { id, text } => {
            let cursor = get_or_fetch_cursor(state).await;
            vec![Ok(sse_event(&ConversationStreamEnvelopeDto {
                session_id: state.session_id.to_string(),
                cursor: ConversationCursorDto { value: cursor },
                delta: ConversationDeltaDto::StatusItemUpdate { id, text },
            }))]
        },
        ClientNotification::ExtensionRegistryChanged => {
            let cursor = get_or_fetch_cursor(state).await;
            vec![Ok(sse_event(&ConversationStreamEnvelopeDto {
                session_id: state.session_id.to_string(),
                cursor: ConversationCursorDto { value: cursor },
                delta: ConversationDeltaDto::ExtensionRegistryChanged,
            }))]
        },
        ClientNotification::ExtensionCommandResult {
            command_name,
            content,
            is_error,
        } => {
            let cursor = get_or_fetch_cursor(state).await;
            let block_id = format!("cmd-{}", Uuid::new_v4());
            let block = if is_error {
                ConversationBlockDto::Error {
                    id: block_id,
                    message: if content.trim().is_empty() {
                        format!("/{command_name} failed")
                    } else {
                        content
                    },
                }
            } else {
                ConversationBlockDto::SystemNote {
                    id: block_id,
                    text: content,
                }
            };
            vec![Ok(sse_event(&ConversationStreamEnvelopeDto {
                session_id: state.session_id.to_string(),
                cursor: ConversationCursorDto { value: cursor },
                delta: ConversationDeltaDto::AppendBlock { block },
            }))]
        },
        _ => Vec::new(),
    }
}

/// 获取 cursor：优先使用缓存，缓存缺失时查询存储。
async fn get_or_fetch_cursor(state: &mut LiveStreamState) -> String {
    if let Some(ref cursor) = state.cached_cursor {
        cursor.clone()
    } else {
        let cursor = state_cursor(&state.runtime, &state.session_id).await;
        state.cached_cursor = Some(cursor.clone());
        cursor
    }
}

fn update_child_tracking_from_parent_event(state: &mut LiveStreamState, event: &Event) {
    match &event.payload {
        EventPayload::AgentSessionSpawned {
            child_session_id, ..
        } => {
            state
                .child_sessions
                .insert(child_session_id.clone(), child_session_id.clone());
            state
                .leaf_child_sessions
                .insert(child_session_id.clone(), child_session_id.clone());
            state
                .last_child_phase
                .insert(child_session_id.clone(), Phase::Thinking);
        },
        EventPayload::AgentSessionCompleted {
            child_session_id, ..
        }
        | EventPayload::AgentSessionFailed {
            child_session_id, ..
        }
        | EventPayload::AgentSessionRecycled { child_session_id } => {
            if let Some(leaf_child_id) = state.child_sessions.remove(child_session_id) {
                state.leaf_child_sessions.remove(&leaf_child_id);
            }
            state.last_child_phase.remove(child_session_id);
        },
        _ => {},
    }
}

fn child_event_to_agent_update(
    state: &mut LiveStreamState,
    event: &Event,
) -> Option<ConversationDeltaDto> {
    if update_compacted_child_leaf(state, event) {
        return None;
    }

    let initial_child_id = resolve_initial_child_id(state, &event.session_id)?;
    let projection = map_child_phase(&event.payload)?;

    if is_duplicate_child_phase(state, &initial_child_id, &projection) {
        return None;
    }
    state
        .last_child_phase
        .insert(initial_child_id.clone(), projection.phase);

    Some(child_phase_delta(initial_child_id, projection))
}

#[derive(Debug)]
struct ChildPhaseProjection {
    phase: Phase,
    current_tool: Option<String>,
}

fn reverse_child_session_index(
    child_sessions: &HashMap<SessionId, SessionId>,
) -> HashMap<SessionId, SessionId> {
    child_sessions
        .iter()
        .map(|(initial, leaf)| (leaf.clone(), initial.clone()))
        .collect()
}

fn is_tracked_child_event(state: &LiveStreamState, event: &Event) -> bool {
    if state.leaf_child_sessions.contains_key(&event.session_id) {
        return true;
    }
    matches!(
        &event.payload,
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id,
            ..
        } if state.leaf_child_sessions.contains_key(parent_session_id)
    )
}

fn update_compacted_child_leaf(state: &mut LiveStreamState, event: &Event) -> bool {
    let EventPayload::SessionContinuedFromCompaction {
        parent_session_id, ..
    } = &event.payload
    else {
        return false;
    };
    let Some(initial_child_id) = state.leaf_child_sessions.remove(parent_session_id) else {
        return true;
    };
    state
        .child_sessions
        .insert(initial_child_id.clone(), event.session_id.clone());
    state
        .leaf_child_sessions
        .insert(event.session_id.clone(), initial_child_id);
    true
}

fn resolve_initial_child_id(
    state: &LiveStreamState,
    leaf_child_id: &SessionId,
) -> Option<SessionId> {
    state.leaf_child_sessions.get(leaf_child_id).cloned()
}

fn map_child_phase(payload: &EventPayload) -> Option<ChildPhaseProjection> {
    let (phase, current_tool) = match payload {
        EventPayload::TurnStarted | EventPayload::AgentRunStarted => (Phase::Thinking, None),
        EventPayload::AssistantMessageStarted { .. } | EventPayload::AssistantTextDelta { .. } => {
            (Phase::Streaming, None)
        },
        EventPayload::ToolCallStarted { tool_name, .. }
        | EventPayload::ToolCallRequested { tool_name, .. } => {
            (Phase::CallingTool, Some(tool_name.clone()))
        },
        EventPayload::ToolCallCompleted { .. } => (Phase::Thinking, None),
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            (Phase::Idle, None)
        },
        EventPayload::ErrorOccurred { .. } => (Phase::Error, None),
        _ => return None,
    };
    Some(ChildPhaseProjection {
        phase,
        current_tool,
    })
}

fn is_duplicate_child_phase(
    state: &LiveStreamState,
    initial_child_id: &SessionId,
    projection: &ChildPhaseProjection,
) -> bool {
    projection.current_tool.is_none()
        && state
            .last_child_phase
            .get(initial_child_id)
            .is_some_and(|last| *last == projection.phase)
}

fn child_phase_delta(
    initial_child_id: SessionId,
    projection: ChildPhaseProjection,
) -> ConversationDeltaDto {
    ConversationDeltaDto::AgentSessionUpdated {
        agent_session: AgentSessionLinkDto::phase_only(
            initial_child_id,
            projection.phase,
            projection.current_tool,
        ),
    }
}

fn event_adds_message(event: &Event) -> bool {
    matches!(
        event.payload,
        EventPayload::UserMessage { .. } | EventPayload::AssistantMessageCompleted { .. }
    )
}

fn sse_event<T: serde::Serialize>(value: &T) -> SseEvent {
    let data = match serde_json::to_string(value) {
        Ok(data) => data,
        Err(error) => {
            tracing::error!(%error, "failed to serialize SSE conversation envelope");
            r#"{"error":"serialization_failed"}"#.to_string()
        },
    };
    SseEvent::default().event("conversation").data(data)
}
