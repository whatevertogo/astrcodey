//! ServerEventBus — 唯一的客户端通知出口。
//!
//! 现在专职做两件事：
//! 1. 把 Session 的事件流（`Session::subscribe`）翻译成 `ClientNotification::Event` 推到
//!    broadcast。通过 `attach(session)` 给目标 session 注册一个 forwarder task。
//! 2. 接收 server 内部产生的非 session 事件（SessionList / Error / SessionResumed 等） 通过
//!    `send_notification` 直接推 broadcast。
//!
//! `ServerEventBus` 不写 EventStore — 持久化由 `Session::emit` / `Session::append_event`
//! 全权负责。这避免了之前 ServerEventBus 与 Session 双写 store 的 bug。

use std::{collections::HashSet, sync::Arc};

use astrcode_core::{storage::EventStore, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::Session;
use parking_lot::Mutex;
use tokio::sync::broadcast;

pub struct ServerEventBus {
    store: Arc<dyn EventStore>,
    tx: broadcast::Sender<ClientNotification>,
    /// 已经 attach 过 forwarder 的 session 集合，防止同 sid 多次 attach 重复广播。
    attached: Mutex<HashSet<SessionId>>,
}

impl ServerEventBus {
    pub fn new(store: Arc<dyn EventStore>, tx: broadcast::Sender<ClientNotification>) -> Self {
        Self {
            store,
            tx,
            attached: Mutex::new(HashSet::new()),
        }
    }

    /// 返回内部 broadcast sender 的引用。
    pub fn broadcast_sender(&self) -> &broadcast::Sender<ClientNotification> {
        &self.tx
    }

    /// 广播任意 ClientNotification（如 SessionResumed、Error 等非 Event 通知）。
    pub fn send_notification(&self, notification: ClientNotification) {
        let _ = self.tx.send(notification);
    }

    /// 把 `session.subscribe()` 上发出的事件转发为 `ClientNotification::Event`。
    ///
    /// 同 sid 多次调用是幂等的：通过内部 `attached` 集合短路。第一次 attach
    /// 创建一个长生命周期的 forwarder task，session 的 broadcast sender drop
    /// 时 task 自然结束。Session 删除（registry 移除）后调用方应调 `detach`
    /// 释放 sid 占位，否则后续同 sid 重建的 session 不会被重新 attach。
    ///
    /// **Lag 处理**：session 内 broadcast 的 capacity 是有限的；一旦本桥的接收
    /// 端跟不上，被丢弃的事件无法补回。此时本 forwarder 主动从 EventStore
    /// 重新拉一份 `SessionResumed` 快照推到下游，触发客户端 rehydrate，避免
    /// UI 与持久状态出现不可恢复的偏差。仅在事件被丢失时才做这次快照。
    pub fn attach(&self, session: &Session) {
        let session_id = session.id().clone();
        if !self.attached.lock().insert(session_id.clone()) {
            return; // 已经 attach 过
        }
        let mut rx = session.subscribe();
        let tx = self.tx.clone();
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let _ = tx.send(ClientNotification::Event(event));
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            session_id = %session_id,
                            skipped = n,
                            "ServerEventBus session forwarder lagged, broadcasting rehydrate snapshot",
                        );
                        emit_rehydrate_snapshot(&store, &tx, &session_id).await;
                    },
                }
            }
        });
    }

    /// 释放 sid 占位。session 被删除后调用，让同 sid 的后续 attach 能重新生效。
    pub fn detach(&self, session_id: &SessionId) {
        self.attached.lock().remove(session_id);
    }

    /// 强制 fsync 指定会话的 durable event log。
    pub async fn sync_durable_events(&self, session_id: &SessionId) {
        if let Err(e) = self.store.sync_durable_events(session_id).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }
}

/// Lag 后用 EventStore 重建快照并以 `SessionResumed` 推下去，让客户端 rehydrate。
///
/// 失败（session 已删除、存储瞬时错误）时仅记日志：此处属于「补救路径」，再失败
/// 就只能让上层超时/重连兜底，不应反复重试以免风暴。
async fn emit_rehydrate_snapshot(
    store: &Arc<dyn EventStore>,
    tx: &broadcast::Sender<ClientNotification>,
    session_id: &SessionId,
) {
    match store.session_read_model(session_id).await {
        Ok(state) => {
            let snapshot = crate::handler::snapshot::session_snapshot(&state);
            let _ = tx.send(ClientNotification::SessionResumed {
                session_id: session_id.to_string(),
                snapshot,
            });
        },
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                error = %e,
                "failed to build rehydrate snapshot after lag",
            );
        },
    }
}
