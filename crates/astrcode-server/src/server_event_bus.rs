//! ServerEventBus — 持久化到 EventStore + 广播到客户端。

use std::sync::Arc;

use astrcode_core::{event::Event, storage::EventStore, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::EventBus;
use tokio::sync::broadcast;

pub struct ServerEventBus {
    store: Arc<dyn EventStore>,
    tx: broadcast::Sender<ClientNotification>,
}

impl ServerEventBus {
    pub fn new(store: Arc<dyn EventStore>, tx: broadcast::Sender<ClientNotification>) -> Self {
        Self { store, tx }
    }
}

#[async_trait::async_trait]
impl EventBus for ServerEventBus {
    async fn emit(&self, session_id: &SessionId, payload: astrcode_core::event::EventPayload) {
        let event = Event::new(session_id.clone(), None, payload);
        if let Err(e) = self.store.append_event(event.clone()).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to persist event via EventBus");
            return;
        }
        let _ = self.tx.send(ClientNotification::Event(event));
    }
}
