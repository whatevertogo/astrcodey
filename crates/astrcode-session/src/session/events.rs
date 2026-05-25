use astrcode_core::{
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    storage::SessionReadModel,
    types::*,
};
use astrcode_support::perf_snapshot;

use super::{Session, SessionError};
use crate::{session_runtime_services::SessionRuntimeServices, turn_context::SharedTurnContext};

impl Session {
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        perf_snapshot::capture_event("session.append_event", &stored);
        Ok(stored)
    }

    pub async fn emit_live(&self, turn_id: Option<&TurnId>, payload: EventPayload) {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
        perf_snapshot::capture_event("session.emit_live", &event);
        self.runtime.fanout(event);
    }

    pub async fn emit_durable(
        &self,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) -> Result<Event, SessionError> {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        perf_snapshot::capture_event("session.emit_durable", &stored);
        Ok(stored)
    }

    pub async fn emit_lifecycle(&self, event: ExtensionEvent) -> Result<(), SessionError> {
        let model = self.read_model().await?;
        emit_lifecycle_for_read_model(&self.caps, &self.id, &model, event).await
    }

    pub async fn update_model_id(&self, model_id: &str) -> Result<Option<Event>, SessionError> {
        let current = self.read_model().await?;
        if current.model_id == model_id {
            return Ok(None);
        }
        self.append_event(Event::new(
            self.id.clone(),
            None,
            EventPayload::ModelIdChanged {
                model_id: model_id.to_string(),
            },
        ))
        .await
        .map(Some)
    }
}

/// 发射 session 生命周期事件，不要求构造完整 [`Session`]。
pub async fn emit_lifecycle_for_read_model(
    caps: &SessionRuntimeServices,
    session_id: &SessionId,
    model: &SessionReadModel,
    event: ExtensionEvent,
) -> Result<(), SessionError> {
    let ctx = SharedTurnContext::from_read_model(session_id, model).lifecycle_ctx();
    caps.extension_runner().emit_lifecycle(event, ctx).await?;
    Ok(())
}
