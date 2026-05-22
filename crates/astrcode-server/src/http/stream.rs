//! `GET /api/sessions/{id}/stream` SSE 流。
//!
//! 三段流串联：
//! 1. `replay_error_stream`：cursor 解析或重放失败时推一条 `RehydrateRequired`。
//! 2. `replay_stream`：从 `EventStore` 拉历史事件转 deltas（按 cursor 起点）。
//! 3. `live_stream`：订阅 `ServerEventBus` 的 broadcast，过滤 sid，推增量。 Lagged 时自发一条
//!    `RehydrateRequired` 让客户端重新拉快照。

use std::sync::Arc;

use astrcode_core::{
    event::Event,
    types::{Cursor, SessionId},
};
use astrcode_protocol::{
    events::ClientNotification,
    http::{ConversationCursorDto, ConversationDeltaDto, ConversationStreamEnvelopeDto},
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

use super::{
    HttpState, error_response,
    projection::{live::event_to_deltas, replay::event_to_replay_deltas},
};
use crate::bootstrap::ServerRuntime;

/// SSE live stream 的内部状态。
///
/// 从 `stream::unfold` 的匿名元组中抽出，提高可读性并方便未来扩展。
struct LiveStreamState {
    rx: mpsc::Receiver<ClientNotification>,
    runtime: Arc<ServerRuntime>,
    session_id: SessionId,
    /// replay 阶段已发送的最大 seq，live 阶段跳过 <= 该值的事件避免重复。
    replay_max_seq: Option<u64>,
    /// Lagged 后设为 true，下一次 poll 返回 None 关闭流。
    closing: bool,
    /// 单个事件产出多条 delta 时，剩余待发送的缓冲。
    pending:
        std::collections::VecDeque<Result<axum::response::sse::Event, std::convert::Infallible>>,
    /// 会话是否已有消息，用于正确计算 can_request_compact。
    has_messages: bool,
    /// 是否已完成初始 stale event 排水。
    /// replay_max_seq 存在时为 false，首次 unfold 调用时执行一次性排水。
    drained: bool,
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
    let read_model = match http_state
        .runtime
        .session_manager
        .read_model(&session_id)
        .await
    {
        Ok(model) => model,
        Err(_) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "Session not found",
            );
        },
    };
    let has_messages = !read_model.messages.is_empty();

    let rx = http_state.event_bus.fanout().subscribe();
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
    let replay_session_id = session_id.clone();
    let replay_has_messages = has_messages;
    let replay_stream = stream::iter(missed_events)
        .then(move |event| {
            let runtime = Arc::clone(&replay_runtime);
            let replay_sid = replay_session_id.clone();
            async move {
                let deltas = event_to_replay_deltas(&event, replay_has_messages);
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
            rx,
            runtime: live_runtime,
            session_id,
            replay_max_seq,
            closing: false,
            pending: std::collections::VecDeque::new(),
            has_messages,
            drained: false,
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
                match state.rx.recv().await {
                    Some(ClientNotification::Event(event))
                        if event.session_id == state.session_id =>
                    {
                        if state
                            .replay_max_seq
                            .zip(event.seq)
                            .is_some_and(|(max_seq, event_seq)| event_seq <= max_seq)
                        {
                            continue;
                        }
                        let deltas = event_to_deltas(&event, state.has_messages);
                        if deltas.is_empty() {
                            continue;
                        }
                        let cursor = event_cursor(&state.runtime, &event).await;
                        let mut items: std::collections::VecDeque<_> = deltas
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
                            .collect();
                        let Some(first) = items.pop_front() else {
                            continue;
                        };
                        state.pending = items;
                        return Some((first, state));
                    },
                    Some(ClientNotification::StatusItemUpdate { id, text }) => {
                        let cursor = state_cursor(&state.runtime, &state.session_id).await;
                        let item = Ok(sse_event(&ConversationStreamEnvelopeDto {
                            session_id: state.session_id.to_string(),
                            cursor: ConversationCursorDto { value: cursor },
                            delta: ConversationDeltaDto::StatusItemUpdate { id, text },
                        }));
                        return Some((item, state));
                    },
                    Some(ClientNotification::ExtensionRegistryChanged) => {
                        let cursor = state_cursor(&state.runtime, &state.session_id).await;
                        let item = Ok(sse_event(&ConversationStreamEnvelopeDto {
                            session_id: state.session_id.to_string(),
                            cursor: ConversationCursorDto { value: cursor },
                            delta: ConversationDeltaDto::ExtensionRegistryChanged,
                        }));
                        return Some((item, state));
                    },
                    Some(_) => {},
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

async fn event_cursor(runtime: &ServerRuntime, event: &Event) -> String {
    if let Some(seq) = event.seq {
        seq.to_string()
    } else {
        state_cursor(runtime, &event.session_id).await
    }
}

async fn state_cursor(runtime: &ServerRuntime, session_id: &SessionId) -> String {
    match runtime.session_manager.latest_cursor(session_id).await {
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

/// 一次性排空 rx 缓冲区，丢弃 replay 时段残留的 live-only 事件。
///
/// 仅在有 replay_max_seq 时有效（即客户端携带 cursor 连接、有历史事件重放）。
/// durable 事件（带 seq）且 seq > replay_max_seq 的保留到 pending。
async fn drain_stale_live_events(state: &mut LiveStreamState) {
    let Some(replay_max) = state.replay_max_seq else {
        return;
    };
    let mut new_durable_events = Vec::new();
    loop {
        match state.rx.try_recv() {
            Ok(ClientNotification::Event(event)) if event.session_id == state.session_id => {
                // live-only 事件（无 seq）属于 replay 时段残留，直接丢弃。
                let Some(seq) = event.seq else {
                    continue;
                };
                // 已被 replay 覆盖的 durable 事件也丢弃。
                if seq <= replay_max {
                    continue;
                }
                new_durable_events.push(event);
            },
            // 非目标 session 事件、StatusItemUpdate、ExtensionRegistryChanged 等丢弃。
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    for event in new_durable_events {
        let deltas = event_to_deltas(&event, state.has_messages);
        if deltas.is_empty() {
            continue;
        }
        let cursor = event_cursor(&state.runtime, &event).await;
        let items: std::collections::VecDeque<_> = deltas
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
            .collect();
        state.pending.extend(items);
    }
}

fn sse_event<T: serde::Serialize>(value: &T) -> SseEvent {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    SseEvent::default().event("conversation").data(data)
}
