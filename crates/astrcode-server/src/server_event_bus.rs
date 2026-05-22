//! ServerEventBus — 唯一的客户端通知出口。
//!
//! 现在专职做两件事：
//! 1. 把 Session 的事件流（`Session::subscribe`）翻译成 `ClientNotification::Event` 推到 fan-out
//!    通道。通过 `attach(session)` 给目标 session 注册一个 forwarder task。
//! 2. 接收 server 内部产生的非 session 事件（SessionList / Error 等）通过 `send_notification`
//!    直接推 fan-out 通道。
//!
//! `ServerEventBus` 不写 EventStore — 持久化由 `Session::emit` / `Session::append_event`
//! 全权负责。这避免了之前 ServerEventBus 与 Session 双写 store 的 bug。

use std::{collections::HashSet, sync::Arc};

use astrcode_core::{storage::EventStore, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::Session;
use astrcode_support::event_fanout::EventFanout;
use parking_lot::Mutex;

pub struct ServerEventBus {
    store: Arc<dyn EventStore>,
    tx: Arc<EventFanout<ClientNotification>>,
    /// 已经 attach 过 forwarder 的 session 集合，防止同 sid 多次 attach 重复广播。
    attached: Mutex<HashSet<SessionId>>,
}

impl ServerEventBus {
    pub fn new(store: Arc<dyn EventStore>, tx: Arc<EventFanout<ClientNotification>>) -> Self {
        Self {
            store,
            tx,
            attached: Mutex::new(HashSet::new()),
        }
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
    /// 使用 unbounded mpsc fan-out，不会发生 Lagged 丢事件。
    pub fn attach(&self, session: &Session) {
        let session_id = session.id().clone();
        if !self.attached.lock().insert(session_id.clone()) {
            return; // 已经 attach 过
        }
        let mut rx = session.subscribe();
        let tx = Arc::clone(&self.tx);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                tx.send(ClientNotification::Event(event));
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
