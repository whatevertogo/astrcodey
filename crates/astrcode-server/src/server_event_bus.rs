//! ServerEventBus — 持久化到 EventStore + 广播到客户端。

use std::sync::Arc;

use astrcode_core::{event::Event, storage::EventStore, types::SessionId, types::TurnId};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::EventBus;
use tokio::sync::broadcast;

pub struct ServerEventBus {
    store: Arc<dyn EventStore>,
    tx: broadcast::Sender<ClientNotification>,
    turn_id: Option<TurnId>,
}

impl ServerEventBus {
    pub fn new(store: Arc<dyn EventStore>, tx: broadcast::Sender<ClientNotification>) -> Self {
        Self {
            store,
            tx,
            turn_id: None,
        }
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }
}

#[async_trait::async_trait]
impl EventBus for ServerEventBus {
    async fn emit(&self, session_id: &SessionId, payload: astrcode_core::event::EventPayload) {
        let event = Event::new(session_id.clone(), self.turn_id.clone(), payload);
        if event.payload.is_durable() {
            if let Err(e) = self.store.append_event(event.clone()).await {
                tracing::error!(session_id = %session_id, error = %e, "failed to persist event via EventBus");
                return;
            }
        }
        let _ = self.tx.send(ClientNotification::Event(event));
    }
}
