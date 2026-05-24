//! ServerEventBus — 唯一的客户端通知出口。
//!
//! 现在专职做三件事：
//! 1. 把 Session 的事件流（`Session::subscribe`）翻译成 `ClientNotification::Event` 推到 fan-out
//!    通道。通过 `attach(session)` 给目标 session 注册一个 forwarder task。
//! 2. 接收 server 内部产生的非 session 事件（SessionList / Error 等）通过 `send_notification`
//!    直接推 fan-out 通道。
//! 3. 在 forwarder 中维护 per-session 的 streaming 消息快照，供 HTTP 重连查询。
//!
//! `ServerEventBus` 不写 EventStore — 持久化由 `Session::emit` / `Session::append_event`
//! 全权负责。这避免了之前 ServerEventBus 与 Session 双写 store 的 bug。

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    event::EventPayload,
    types::{MessageId, SessionId},
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::Session;
use astrcode_support::event_fanout::EventFanout;
use parking_lot::Mutex;

use crate::turn_scheduler::TurnScheduler;

/// Streaming 消息的瞬时快照，供 HTTP 层构建重连响应。
pub(crate) struct StreamingSnapshot {
    pub message_id: String,
    pub text: String,
    pub reasoning_content: Option<String>,
}

/// Forwarder 持有的 per-session streaming 内部状态。
type StreamingState = parking_lot::Mutex<Option<(MessageId, String, String)>>;

pub struct ServerEventBus {
    tx: Arc<EventFanout<ClientNotification>>,
    /// 已经 attach 过 forwarder 的 session 集合，防止同 sid 多次 attach 重复广播。
    attached: Mutex<HashSet<SessionId>>,
    /// per-session 的 streaming 状态，由 forwarder 从 live 事件流维护。
    /// 外层 Mutex 保护 HashMap 结构；内层 Arc<Mutex> 让 forwarder 无需竞争整个 map。
    streaming: Mutex<HashMap<SessionId, Arc<StreamingState>>>,
    /// TurnScheduler 引用，用于后台任务完成后触发继续处理。
    /// 使用 OnceCell 避免构造时的循环依赖问题。
    scheduler: tokio::sync::OnceCell<Arc<TurnScheduler>>,
}

impl ServerEventBus {
    pub fn new(tx: Arc<EventFanout<ClientNotification>>) -> Self {
        Self {
            tx,
            attached: Mutex::new(HashSet::new()),
            streaming: Mutex::new(HashMap::new()),
            scheduler: tokio::sync::OnceCell::new(),
        }
    }

    /// 绑定 TurnScheduler，用于后台任务完成后触发继续处理。
    /// 此方法应在 Server 构造完成后调用，解决构造时的循环依赖问题。
    pub async fn bind_scheduler(&self, scheduler: Arc<TurnScheduler>) {
        let _ = self.scheduler.set(scheduler);
    }

    /// 返回内部 fan-out 通道的引用。
    pub fn fanout(&self) -> &EventFanout<ClientNotification> {
        &self.tx
    }

    /// 广播任意 ClientNotification（如 Error 等非 Event 通知）。
    pub fn send_notification(&self, notification: ClientNotification) {
        self.tx.send(notification);
    }

    /// 把 `session.subscribe()` 上发出的事件转发为 `ClientNotification::Event`。
    ///
    /// 同 sid 多次调用是幂等的：通过内部 `attached` 集合短路。第一次 attach
    /// 创建一个长生命周期的 forwarder task，session 的 sender drop
    /// 时 task 自然结束。Session 删除（registry 移除）后调用方应调 `detach`
    /// 释放 sid 占位，否则后续同 sid 重建的 session 不会被重新 attach。
    ///
    /// ## 后台任务处理
    /// 当检测到 `BackgroundTaskCompleted` 事件时，会调用 TurnScheduler 的
    /// `notify_step` 方法，在当前 turn 的下一步继续处理。
    /// 
    /// 当检测到 `TurnCompleted` 事件时，会调用 `on_turn_completed` 检查并处理
    /// 队列中等待的 "下一 turn" 消息。
    pub fn attach(&self, session: &Session) {
        let session_id = session.id().clone();
        if !self.attached.lock().insert(session_id.clone()) {
            return;
        }
        let mut rx = session.subscribe();
        let tx = Arc::clone(&self.tx);
        let scheduler = self.scheduler.get().cloned();
        let state = Arc::clone(
            self.streaming
                .lock()
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(StreamingState::new(None))),
        );
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // 处理后台任务完成事件，触发继续处理
                if matches!(event.payload, EventPayload::BackgroundTaskCompleted { .. }) {
                    if let Some(ref scheduler) = scheduler {
                        // 使用 notify_step 路径：立即在当前 turn 下一步处理
                        if let Err(e) = scheduler.notify_step(session_id.clone(), "task").await {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "failed to notify background completion (step path)"
                            );
                        }
                    }
                }
                
                // 处理 turn 完成事件，检查是否有队列中的待处理消息
                if matches!(event.payload, EventPayload::TurnCompleted { .. }) {
                    if let Some(ref scheduler) = scheduler {
                        scheduler.on_turn_completed(&session_id).await;
                    }
                }
                
                update_streaming(&state, &event.payload);
                tx.send(ClientNotification::Event(event));
            }
        });
    }

    /// 释放 sid 占位。session 被删除后调用，让同 sid 的后续 attach 能重新生效。
    pub fn detach(&self, session_id: &SessionId) {
        self.attached.lock().remove(session_id);
        self.streaming.lock().remove(session_id);
    }

    /// 返回当前 streaming 消息的瞬时快照。
    pub(crate) fn streaming_snapshot(&self, session_id: &SessionId) -> Option<StreamingSnapshot> {
        self.streaming.lock().get(session_id).and_then(|state| {
            state
                .lock()
                .as_ref()
                .map(|(id, text, reasoning)| StreamingSnapshot {
                    message_id: id.to_string(),
                    text: text.clone(),
                    reasoning_content: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning.clone())
                    },
                })
        })
    }
}

/// 从事件 payload 同步更新 streaming 状态。
fn update_streaming(state: &StreamingState, payload: &EventPayload) {
    let mut guard = state.lock();
    match payload {
        EventPayload::AssistantMessageStarted { message_id } => {
            *guard = Some((message_id.clone(), String::new(), String::new()));
        },
        EventPayload::AssistantTextDelta { delta, .. } => {
            if let Some((_, text, _)) = guard.as_mut() {
                text.push_str(delta);
            }
        },
        EventPayload::ThinkingDelta { delta, .. } => {
            if let Some((_, _, reasoning)) = guard.as_mut() {
                reasoning.push_str(delta);
            }
        },
        EventPayload::AssistantMessageCompleted { .. } | EventPayload::TurnCompleted { .. } => {
            *guard = None;
        },
        _ => {},
    }
}
