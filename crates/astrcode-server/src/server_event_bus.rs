//! 客户端事件 fan-out。
//!
//! Session 事件按 conversation 分发，非事件通知走全局通道。

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    event::{Event, EventPayload},
    types::{MessageId, SessionId},
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::Session;
use astrcode_support::event_fanout::EventFanout;
use parking_lot::Mutex;
use tokio::sync::mpsc;

pub(crate) struct StreamingSnapshot {
    pub message_id: String,
    pub text: String,
    pub reasoning_content: Option<String>,
}

type StreamingState = parking_lot::Mutex<Option<(MessageId, String, String)>>;

pub struct ServerEventBus {
    legacy_tx: Option<Arc<EventFanout<ClientNotification>>>,
    global_notifications: Arc<EventFanout<ClientNotification>>,
    conversation_events: Mutex<HashMap<SessionId, Arc<EventFanout<Arc<Event>>>>>,
    session_roots: Mutex<HashMap<SessionId, SessionId>>,
    attached: Mutex<HashSet<SessionId>>,
    streaming: Mutex<HashMap<SessionId, Arc<StreamingState>>>,
}

impl ServerEventBus {
    const EVENT_FANOUT_CAPACITY: usize = 1024;

    pub fn new() -> Self {
        Self::new_with_legacy_tx(None)
    }

    pub fn with_legacy_tx(legacy_tx: Arc<EventFanout<ClientNotification>>) -> Self {
        Self::new_with_legacy_tx(Some(legacy_tx))
    }

    fn new_with_legacy_tx(legacy_tx: Option<Arc<EventFanout<ClientNotification>>>) -> Self {
        Self {
            legacy_tx,
            global_notifications: Arc::new(EventFanout::new(Self::EVENT_FANOUT_CAPACITY)),
            conversation_events: Mutex::new(HashMap::new()),
            session_roots: Mutex::new(HashMap::new()),
            attached: Mutex::new(HashSet::new()),
            streaming: Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe_global_notifications(&self) -> mpsc::Receiver<ClientNotification> {
        self.global_notifications.subscribe()
    }

    pub fn subscribe_conversation_events(
        &self,
        session_id: &SessionId,
    ) -> mpsc::Receiver<Arc<Event>> {
        self.conversation_fanout(session_id).subscribe()
    }

    pub(crate) fn register_conversation_children(
        &self,
        conversation_session_id: &SessionId,
        child_sessions: &HashMap<SessionId, SessionId>,
    ) {
        if child_sessions.is_empty() {
            return;
        }

        let mut roots = self.session_roots.lock();
        roots
            .entry(conversation_session_id.clone())
            .or_insert_with(|| conversation_session_id.clone());
        for (initial_child_id, leaf_child_id) in child_sessions {
            roots.insert(initial_child_id.clone(), conversation_session_id.clone());
            roots.insert(leaf_child_id.clone(), conversation_session_id.clone());
        }
    }

    pub fn send_notification(&self, notification: ClientNotification) {
        match notification {
            ClientNotification::Event(event) => self.publish_event(Arc::new(event)),
            notification => {
                self.global_notifications.send(notification.clone());
                if let Some(legacy_tx) = &self.legacy_tx {
                    legacy_tx.send(notification);
                }
            },
        }
    }

    pub fn publish_event(&self, event: Arc<Event>) {
        let session_deleted = matches!(event.payload, EventPayload::SessionDeleted);
        let root_session_id = self.conversation_root_for_event(&event);
        self.remember_event_routes(&event, &root_session_id);
        self.update_streaming_snapshot(&event);
        self.send_to_existing_conversation_fanout(&event.session_id, Arc::clone(&event));
        if root_session_id != event.session_id {
            self.send_to_existing_conversation_fanout(&root_session_id, Arc::clone(&event));
        }
        if let Some(legacy_tx) = &self.legacy_tx {
            legacy_tx.send(ClientNotification::Event((*event).clone()));
        }
        if session_deleted {
            self.attached.lock().remove(&event.session_id);
            self.forget_session_routes(&event.session_id);
        }
    }

    pub fn attach(self: &Arc<Self>, session: &Session) {
        self.attach_client_fanout(session);
    }

    fn attach_client_fanout(self: &Arc<Self>, session: &Session) {
        let session_id = session.id().clone();
        if !self.attached.lock().insert(session_id) {
            return;
        }
        let mut rx = session.subscribe();
        let event_bus = Arc::clone(self);
        crate::task_utils::spawn_traced("server_event_bus_fanout", async move {
            while let Some(event) = rx.recv().await {
                event_bus.publish_event(event);
            }
        });
    }

    pub fn detach(&self, session_id: &SessionId) {
        self.attached.lock().remove(session_id);
        self.forget_session_routes(session_id);
    }

    fn forget_session_routes(&self, session_id: &SessionId) {
        self.streaming.lock().remove(session_id);
        self.conversation_events.lock().remove(session_id);
        self.session_roots
            .lock()
            .retain(|session, root| session != session_id && root != session_id);
    }

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

    fn conversation_fanout(&self, session_id: &SessionId) -> Arc<EventFanout<Arc<Event>>> {
        Arc::clone(
            self.conversation_events
                .lock()
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(EventFanout::new(Self::EVENT_FANOUT_CAPACITY))),
        )
    }

    fn existing_conversation_fanout(
        &self,
        session_id: &SessionId,
    ) -> Option<Arc<EventFanout<Arc<Event>>>> {
        self.conversation_events
            .lock()
            .get(session_id)
            .map(Arc::clone)
    }

    fn send_to_existing_conversation_fanout(&self, session_id: &SessionId, event: Arc<Event>) {
        if let Some(fanout) = self.existing_conversation_fanout(session_id) {
            fanout.send(event);
        }
    }

    fn conversation_root_for_event(&self, event: &Event) -> SessionId {
        match &event.payload {
            EventPayload::SessionStarted {
                parent_session_id: Some(parent_session_id),
                ..
            }
            | EventPayload::SessionContinuedFromCompaction {
                parent_session_id, ..
            } => self.session_root(parent_session_id),
            _ => self.session_root(&event.session_id),
        }
    }

    fn session_root(&self, session_id: &SessionId) -> SessionId {
        self.session_roots
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.clone())
    }

    fn remember_event_routes(&self, event: &Event, root_session_id: &SessionId) {
        let mut roots = self.session_roots.lock();
        roots
            .entry(event.session_id.clone())
            .or_insert_with(|| root_session_id.clone());
        match &event.payload {
            EventPayload::SessionStarted {
                parent_session_id: None,
                ..
            } => {
                roots.insert(event.session_id.clone(), event.session_id.clone());
            },
            EventPayload::SessionStarted {
                parent_session_id: Some(_),
                ..
            }
            | EventPayload::SessionContinuedFromCompaction { .. } => {
                roots.insert(event.session_id.clone(), root_session_id.clone());
            },
            EventPayload::AgentSessionSpawned {
                child_session_id, ..
            } => {
                roots.insert(child_session_id.clone(), root_session_id.clone());
            },
            _ => {},
        }
    }

    fn update_streaming_snapshot(&self, event: &Event) {
        let state = Arc::clone(
            self.streaming
                .lock()
                .entry(event.session_id.clone())
                .or_insert_with(|| Arc::new(StreamingState::new(None))),
        );
        update_streaming(&state, &event.payload);
    }
}

impl Default for ServerEventBus {
    fn default() -> Self {
        Self::new()
    }
}

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

#[cfg(test)]
mod tests {
    use astrcode_core::{event::EventPayload, types::SessionId};

    use super::*;

    fn turn_started(session_id: &SessionId) -> Arc<Event> {
        Arc::new(Event::session(
            session_id.clone(),
            EventPayload::TurnStarted,
        ))
    }

    #[test]
    fn publish_without_subscriber_does_not_create_conversation_fanout() {
        let bus = ServerEventBus::new();
        let session_id = SessionId::new("session-1");

        bus.publish_event(turn_started(&session_id));

        assert!(bus.existing_conversation_fanout(&session_id).is_none());
    }

    #[tokio::test]
    async fn subscribed_conversation_receives_published_event() {
        let bus = ServerEventBus::new();
        let session_id = SessionId::new("session-1");
        let mut rx = bus.subscribe_conversation_events(&session_id);

        bus.publish_event(turn_started(&session_id));

        let event = rx.recv().await.expect("conversation event");
        assert_eq!(event.session_id, session_id);
    }

    #[tokio::test]
    async fn legacy_event_broadcast_is_only_sent_when_enabled() {
        let legacy_tx = Arc::new(EventFanout::new(8));
        let bus = ServerEventBus::with_legacy_tx(Arc::clone(&legacy_tx));
        let session_id = SessionId::new("session-1");
        let mut legacy_rx = legacy_tx.subscribe();

        bus.publish_event(turn_started(&session_id));

        let notification = legacy_rx.recv().await.expect("legacy event");
        match notification {
            ClientNotification::Event(event) => assert_eq!(event.session_id, session_id),
            other => panic!("expected event notification, got {other:?}"),
        }
    }
}
