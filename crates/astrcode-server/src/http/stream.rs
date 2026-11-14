//! `GET /api/sessions/{id}/stream` SSE 流。
//!
//! 三段流串联：
//! 1. `replay_error_stream`：cursor 解析或重放失败时推一条 `RehydrateRequired`。
//! 2. `replay_stream`：从 EventLog history 拉事件转 deltas（按 cursor 起点）。
//! 3. `live_stream`：订阅当前 conversation 的事件广播与全局非事件通知。

mod child_sessions;
mod replay;

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event, EventPayload, Phase},
    types::SessionId,
};
use astrcode_protocol::{
    events::ClientNotification,
    http::{
        ConversationBlockDto, ConversationCursorDto, ConversationDeltaDto,
        ConversationStreamEnvelopeDto,
    },
};
use astrcode_session_projection::AgentSessionStatus;
use axum::{
    extract::{Path, Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
};
use child_sessions::ChildSessionTracker;
use futures_util::{StreamExt, stream};
use replay::{ReplayedSettlements, parse_replay_cursor, replay_after_cursor};
use serde::Deserialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{
    HttpState,
    projection::{live::event_to_deltas, replay::event_to_replay_deltas},
    session_read_error_response,
};

type SseItem = Result<axum::response::sse::Event, std::convert::Infallible>;

/// SSE live stream 的内部状态。
///
/// 从 `stream::unfold` 的匿名元组中抽出，提高可读性并方便未来扩展。
struct LiveStreamState {
    event_rx: broadcast::Receiver<Arc<Event>>,
    notification_rx: broadcast::Receiver<ClientNotification>,
    session_id: SessionId,
    /// 已有快照或 replay 覆盖的位置，live 阶段跳过 <= 该值的持久事件。
    replay_max_seq: Option<u64>,
    replayed_settlements: ReplayedSettlements,
    /// 需要主动关闭流时设为 true，下一次 poll 返回 None。
    closing: bool,
    /// 单个事件产出多条 delta 时，剩余待发送的缓冲。
    pending: std::collections::VecDeque<SseItem>,
    /// 会话是否已有消息，用于正确计算 can_request_compact。
    has_messages: bool,
    /// 是否已完成初始 stale event 排水。
    drained: bool,
    child_sessions: ChildSessionTracker,
    /// 本连接处理过的父会话持久位置。实时帧和子会话帧只沿用此值。
    cursor: u64,
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
        .app
        .runtime()
        .session_manager()
        .has_messages(&session_id)
        .await
    {
        Ok(has_messages) => has_messages,
        Err(error) => return session_read_error_response(error),
    };
    let agent_sessions = match http_state
        .app
        .runtime()
        .session_manager()
        .agent_sessions(&session_id)
        .await
    {
        Ok(agent_sessions) => agent_sessions,
        Err(error) => return session_read_error_response(error),
    };
    let child_sessions: HashMap<SessionId, SessionId> = agent_sessions
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
    let last_child_phase = agent_sessions
        .iter()
        .filter(|link| link.status == AgentSessionStatus::Running)
        .map(|link| (link.child_session_id.clone(), Phase::Thinking))
        .collect();
    let child_session_tracker = ChildSessionTracker::new(child_sessions.clone(), last_child_phase);

    http_state
        .app
        .event_bus()
        .register_conversation_children(&session_id, &child_sessions);
    let event_rx = http_state
        .app
        .event_bus()
        .subscribe_conversation_events(&session_id);
    let notification_rx = http_state.app.event_bus().subscribe_global_notifications();
    let requested_cursor = query.cursor.as_deref().map(parse_replay_cursor).transpose();
    let replay_result = match requested_cursor {
        Ok(Some(cursor)) => replay_after_cursor(http_state.app.runtime(), &session_id, cursor)
            .await
            .map(|events| (events, Some(cursor))),
        Ok(None) => Ok((Vec::new(), None)),
        Err(error) => Err(error),
    };
    let (missed_events, requested_seq, replay_error) = match replay_result {
        Ok((events, cursor)) => (events, cursor, false),
        Err(error) => {
            if error.is_cursor_unavailable() {
                tracing::info!(%session_id, %error, "SSE cursor requires rehydrate");
            } else {
                tracing::warn!(%session_id, %error, "failed to replay SSE cursor");
            }
            (Vec::new(), None, true)
        },
    };
    let replay_max_seq = missed_events
        .iter()
        .filter_map(|event| event.seq)
        .max()
        .or(requested_seq);
    let replayed_settlements = ReplayedSettlements::new(&missed_events);
    let replay_session_id = session_id.clone();
    let mut replay_cursor = requested_seq.unwrap_or(0);
    let replay_stream = stream::iter(missed_events)
        .map(move |event| {
            conversation_event_items(
                &replay_session_id,
                &mut replay_cursor,
                event.seq,
                event_to_replay_deltas(&event, has_messages),
            )
        })
        .flat_map(stream::iter);
    let replay_error_stream =
        stream::iter(replay_error.then(|| {
            conversation_sse_item(&session_id, 0, ConversationDeltaDto::RehydrateRequired)
        }));

    let live_stream = stream::unfold(
        LiveStreamState {
            event_rx,
            notification_rx,
            session_id,
            replay_max_seq,
            replayed_settlements,
            closing: false,
            pending: std::collections::VecDeque::new(),
            has_messages,
            drained: false,
            cursor: replay_max_seq.unwrap_or(0),
            child_sessions: child_session_tracker,
        },
        |mut state| async move {
            if state.closing {
                return None;
            }

            if !state.drained {
                state.drained = true;
                drain_stale_live_events(&mut state);
            }
            if state.closing {
                return None;
            }

            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }

            loop {
                match recv_live_input(&mut state).await {
                    Some(input) => {
                        let mut items: std::collections::VecDeque<_> =
                            live_input_to_sse_items(&mut state, input).into();

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
    // Rehydrate switches the client to a new snapshot/stream generation. Do not
    // poll the old live receiver after sending the marker, otherwise it could
    // deliver deltas while the replacement snapshot is being installed.
    let live_stream = live_stream.take(if replay_error { 0 } else { usize::MAX });
    let connected_stream = stream::iter([Ok(SseEvent::default().comment("connected"))]);
    let stream = connected_stream
        .chain(replay_error_stream)
        .chain(replay_stream)
        .chain(live_stream);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn recv_live_input(state: &mut LiveStreamState) -> Option<LiveInput> {
    // Conversation events and global notifications are intentionally separate
    // channels. We preserve ordering inside each channel, but do not promise a
    // total order across them.
    tokio::select! {
        event = state.event_rx.recv() => event.ok().map(LiveInput::Event),
        notification = state.notification_rx.recv() => {
            notification.ok().map(|notification| LiveInput::Notification(Box::new(notification)))
        },
    }
}

/// 首次切到实时流时过滤已回放的事件，以及这些事件已结算的临时输出。
fn drain_stale_live_events(state: &mut LiveStreamState) {
    let Some(replay_max) = state.replay_max_seq else {
        return;
    };
    let mut buffered = Vec::new();
    loop {
        match state.event_rx.try_recv() {
            Ok(event) => {
                if event.session_id == state.session_id {
                    let Some(seq) = event.seq else {
                        if !state.replayed_settlements.contains(&event) {
                            buffered.push(LiveInput::Event(event));
                        }
                        continue;
                    };
                    // 已被 replay 覆盖的 durable 事件也丢弃。
                    if seq <= replay_max {
                        continue;
                    }
                    buffered.push(LiveInput::Event(event));
                } else if state.child_sessions.is_tracked_event(&event) {
                    buffered.push(LiveInput::Event(event));
                }
            },
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(
                broadcast::error::TryRecvError::Closed | broadcast::error::TryRecvError::Lagged(_),
            ) => {
                state.closing = true;
                return;
            },
        }
    }
    loop {
        match state.notification_rx.try_recv() {
            Ok(notification) => match notification {
                ClientNotification::StatusItemUpdate { .. }
                | ClientNotification::ExtensionRegistryChanged
                | ClientNotification::ExtensionCommandResult { .. }
                | ClientNotification::GlobalCustomEvent { .. } => {
                    buffered.push(LiveInput::Notification(Box::new(notification)));
                },
                _ => {},
            },
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(
                broadcast::error::TryRecvError::Closed | broadcast::error::TryRecvError::Lagged(_),
            ) => {
                state.closing = true;
                return;
            },
        }
    }
    for input in buffered {
        let items = live_input_to_sse_items(state, input);
        state.pending.extend(items);
    }
}

fn live_input_to_sse_items(state: &mut LiveStreamState, input: LiveInput) -> Vec<SseItem> {
    match input {
        LiveInput::Event(event) => event_to_sse_items(state, event),
        LiveInput::Notification(notification) => notification_to_sse_items(state, *notification),
    }
}

fn event_to_sse_items(state: &mut LiveStreamState, event: Arc<Event>) -> Vec<SseItem> {
    match event.as_ref() {
        event if event.session_id == state.session_id => {
            if already_replayed(state.replay_max_seq, event.seq)
                || state.replayed_settlements.contains(event)
            {
                return Vec::new();
            }
            if event_adds_message(event) {
                state.has_messages = true;
            }
            state.child_sessions.update_from_parent_event(event);

            conversation_event_items(
                &state.session_id,
                &mut state.cursor,
                event.seq,
                event_to_deltas(event, state.has_messages),
            )
        },
        event => {
            let Some(delta) = state.child_sessions.project_event(event) else {
                return Vec::new();
            };
            // 子事件的 seq 属于子会话，不能用来更新父会话的 cursor
            vec![conversation_sse_item(
                &state.session_id,
                state.cursor,
                delta,
            )]
        },
    }
}

fn notification_to_sse_items(
    state: &LiveStreamState,
    notification: ClientNotification,
) -> Vec<SseItem> {
    let delta = match notification {
        ClientNotification::Event(_) => return Vec::new(),
        ClientNotification::StatusItemUpdate { id, text } => {
            ConversationDeltaDto::StatusItemUpdate { id, text }
        },
        ClientNotification::ExtensionRegistryChanged => {
            ConversationDeltaDto::ExtensionRegistryChanged
        },
        ClientNotification::ExtensionCommandResult {
            command_name,
            content,
            is_error,
        } => {
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
            ConversationDeltaDto::AppendBlock { block }
        },
        ClientNotification::GlobalCustomEvent {
            session_id,
            extension_id,
            event_type,
            schema_version,
            payload,
        } => {
            if session_id == state.session_id.as_str() {
                return Vec::new();
            }
            ConversationDeltaDto::CustomEvent {
                extension_id,
                event_type,
                schema_version,
                payload,
            }
        },
        _ => return Vec::new(),
    };

    vec![conversation_sse_item(
        &state.session_id,
        state.cursor,
        delta,
    )]
}

/// 事件是否已被 replay 阶段覆盖(durable 事件 seq 不超过 replay 已发送的最大 seq)。
fn already_replayed(replay_max_seq: Option<u64>, event_seq: Option<u64>) -> bool {
    replay_max_seq
        .zip(event_seq)
        .is_some_and(|(max, seq)| seq <= max)
}

fn event_adds_message(event: &Event) -> bool {
    matches!(
        event.payload,
        EventPayload::Durable(
            DurableEventPayload::UserMessage { .. } | DurableEventPayload::AssistantMessage { .. }
        )
    )
}

fn conversation_event_items(
    session_id: &SessionId,
    cursor: &mut u64,
    seq: Option<u64>,
    deltas: Vec<ConversationDeltaDto>,
) -> Vec<SseItem> {
    let previous_cursor = *cursor;
    if let Some(seq) = seq {
        *cursor = seq;
    }
    // A reconnect between deltas must replay the whole durable event.
    let count = deltas.len();
    deltas
        .into_iter()
        .enumerate()
        .map(|(index, delta)| {
            let frame_cursor = if index + 1 == count {
                *cursor
            } else {
                previous_cursor
            };
            conversation_sse_item(session_id, frame_cursor, delta)
        })
        .collect()
}

fn conversation_sse_item(
    session_id: &SessionId,
    cursor: u64,
    delta: ConversationDeltaDto,
) -> SseItem {
    Ok(sse_event(&ConversationStreamEnvelopeDto {
        session_id: session_id.to_string(),
        cursor: ConversationCursorDto {
            value: cursor.to_string(),
        },
        delta,
    }))
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
