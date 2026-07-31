//! 客户端事件 fan-out。
//!
//! Session 事件按 conversation 分发，非事件通知走全局通道。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event, EventPayload, LiveEventPayload, Phase},
    types::{MessageId, SessionId},
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::SessionEventObserver;
use astrcode_session_projection::SessionReadModel;
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::protocol_mapping::session_snapshot;

pub(crate) struct StreamingSnapshot {
    pub message_id: String,
    pub text: String,
    pub reasoning_content: Option<String>,
}

type StreamingState = parking_lot::Mutex<Option<(MessageId, String, String)>>;

#[derive(Clone)]
enum SessionRoute {
    Conversation(SessionId),
    AgentChild(SessionId),
}

impl SessionRoute {
    fn root_session_id(&self) -> &SessionId {
        match self {
            Self::Conversation(session_id) | Self::AgentChild(session_id) => session_id,
        }
    }

    fn forwards_to_root(&self, payload: &EventPayload) -> bool {
        match self {
            Self::Conversation(_) => true,
            Self::AgentChild(_) => agent_session_progress(payload).is_some(),
        }
    }
}

pub struct ServerEventBus {
    all_notifications: broadcast::Sender<ClientNotification>,
    global_notifications: broadcast::Sender<ClientNotification>,
    conversation_events: Mutex<HashMap<SessionId, broadcast::Sender<Arc<Event>>>>,
    session_routes: Mutex<HashMap<SessionId, SessionRoute>>,
    streaming: Mutex<HashMap<SessionId, Arc<StreamingState>>>,
}

impl ServerEventBus {
    const EVENT_CHANNEL_CAPACITY: usize = 1024;

    pub fn new() -> Self {
        let (all_notifications, _) = broadcast::channel(Self::EVENT_CHANNEL_CAPACITY);
        let (global_notifications, _) = broadcast::channel(Self::EVENT_CHANNEL_CAPACITY);
        Self {
            all_notifications,
            global_notifications,
            conversation_events: Mutex::new(HashMap::new()),
            session_routes: Mutex::new(HashMap::new()),
            streaming: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe to the complete transport-facing notification stream.
    ///
    /// Unlike [`Self::subscribe_global_notifications`], this includes session
    /// events and is intended for transports such as stdio/TUI that expose one
    /// process-wide notification channel.
    pub fn subscribe_all_notifications(&self) -> broadcast::Receiver<ClientNotification> {
        self.all_notifications.subscribe()
    }

    pub fn subscribe_global_notifications(&self) -> broadcast::Receiver<ClientNotification> {
        self.global_notifications.subscribe()
    }

    pub fn subscribe_conversation_events(
        &self,
        session_id: &SessionId,
    ) -> broadcast::Receiver<Arc<Event>> {
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

        let mut routes = self.session_routes.lock();
        routes
            .entry(conversation_session_id.clone())
            .or_insert_with(|| SessionRoute::Conversation(conversation_session_id.clone()));
        for (initial_child_id, leaf_child_id) in child_sessions {
            let route = SessionRoute::AgentChild(conversation_session_id.clone());
            routes.insert(initial_child_id.clone(), route.clone());
            routes.insert(leaf_child_id.clone(), route);
        }
    }

    pub(crate) fn send_session_resumed(&self, state: &SessionReadModel) {
        self.send_notification(ClientNotification::SessionResumed {
            session_id: state.identity.session_id.to_string(),
            snapshot: session_snapshot(state),
        });
    }

    pub(crate) fn send_status_item_update(&self, id: String, text: String) {
        self.send_notification(ClientNotification::StatusItemUpdate { id, text });
    }

    pub(crate) fn send_extension_command_result(
        &self,
        command_name: String,
        content: String,
        is_error: bool,
    ) {
        self.send_notification(ClientNotification::ExtensionCommandResult {
            command_name,
            content,
            is_error,
        });
    }

    pub fn send_notification(&self, notification: ClientNotification) {
        match notification {
            ClientNotification::Event(event) => self.publish_event(Arc::new(event)),
            notification => {
                let _ = self.global_notifications.send(notification.clone());
                let _ = self.all_notifications.send(notification);
            },
        }
    }

    pub fn publish_event(&self, event: Arc<Event>) {
        let route = self.route_for_event(&event);
        self.remember_event_route(&event, &route);
        self.update_streaming_snapshot(&event);
        self.send_to_existing_conversation_fanout(&event.session_id, Arc::clone(&event));
        if route.root_session_id() != &event.session_id && route.forwards_to_root(&event.payload) {
            self.send_to_existing_conversation_fanout(route.root_session_id(), Arc::clone(&event));
        }
        let _ = self
            .all_notifications
            .send(ClientNotification::Event((*event).clone()));
    }

    pub fn detach(&self, session_id: &SessionId) {
        self.forget_session_routes(session_id);
    }

    fn forget_session_routes(&self, session_id: &SessionId) {
        self.streaming.lock().remove(session_id);
        self.conversation_events.lock().remove(session_id);
        self.session_routes.lock().retain(|session, route| {
            session != session_id && route.root_session_id() != session_id
        });
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

    fn conversation_fanout(&self, session_id: &SessionId) -> broadcast::Sender<Arc<Event>> {
        self.conversation_events
            .lock()
            .entry(session_id.clone())
            .or_insert_with(|| broadcast::channel(Self::EVENT_CHANNEL_CAPACITY).0)
            .clone()
    }

    fn existing_conversation_fanout(
        &self,
        session_id: &SessionId,
    ) -> Option<broadcast::Sender<Arc<Event>>> {
        self.conversation_events.lock().get(session_id).cloned()
    }

    fn send_to_existing_conversation_fanout(&self, session_id: &SessionId, event: Arc<Event>) {
        if let Some(fanout) = self.existing_conversation_fanout(session_id) {
            let _ = fanout.send(event);
        }
    }

    fn route_for_event(&self, event: &Event) -> SessionRoute {
        match &event.payload {
            EventPayload::Durable(DurableEventPayload::SessionStarted(started)) => {
                let Some(parent) = &started.parent else {
                    return self.route_for_session(&event.session_id);
                };
                SessionRoute::AgentChild(
                    self.route_for_session(&parent.session_id)
                        .root_session_id()
                        .clone(),
                )
            },
            _ => self.route_for_session(&event.session_id),
        }
    }

    fn route_for_session(&self, session_id: &SessionId) -> SessionRoute {
        self.session_routes
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| SessionRoute::Conversation(session_id.clone()))
    }

    fn remember_event_route(&self, event: &Event, route: &SessionRoute) {
        let mut routes = self.session_routes.lock();
        routes
            .entry(event.session_id.clone())
            .or_insert_with(|| route.clone());
        match &event.payload {
            EventPayload::Durable(DurableEventPayload::SessionStarted(started))
                if started.parent.is_none() =>
            {
                routes.insert(
                    event.session_id.clone(),
                    SessionRoute::Conversation(event.session_id.clone()),
                );
            },
            EventPayload::Durable(DurableEventPayload::SessionStarted(started))
                if started.parent.is_some() =>
            {
                routes.insert(event.session_id.clone(), route.clone());
            },
            EventPayload::Durable(DurableEventPayload::AgentSessionSpawned {
                child_session_id,
                ..
            }) => {
                routes.insert(
                    child_session_id.clone(),
                    SessionRoute::AgentChild(route.root_session_id().clone()),
                );
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

impl SessionEventObserver for ServerEventBus {
    fn publish(&self, event: Arc<Event>) {
        self.publish_event(event);
    }
}

pub(crate) fn agent_session_progress(payload: &EventPayload) -> Option<(Phase, Option<String>)> {
    match payload {
        EventPayload::Durable(payload) => match payload {
            DurableEventPayload::TurnStarted => Some((Phase::Thinking, None)),
            DurableEventPayload::ToolCallRequested { tool_name, .. } => {
                Some((Phase::CallingTool, Some(tool_name.clone())))
            },
            DurableEventPayload::ToolCallCompleted { .. }
            | DurableEventPayload::ToolCallFailed { .. }
            | DurableEventPayload::ToolCallCancelled { .. } => Some((Phase::Thinking, None)),
            DurableEventPayload::TurnCompleted { .. } => Some((Phase::Idle, None)),
            DurableEventPayload::ErrorOccurred { .. } => Some((Phase::Error, None)),
            _ => None,
        },
        EventPayload::Live(payload) => match payload {
            LiveEventPayload::AgentRunStarted => Some((Phase::Thinking, None)),
            LiveEventPayload::AssistantMessageStarted { .. } => Some((Phase::Streaming, None)),
            LiveEventPayload::ToolCallStarted { tool_name, .. } => {
                Some((Phase::CallingTool, Some(tool_name.clone())))
            },
            LiveEventPayload::AgentRunCompleted { .. } => Some((Phase::Idle, None)),
            LiveEventPayload::ErrorOccurred { .. } => Some((Phase::Error, None)),
            _ => None,
        },
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
        EventPayload::Live(LiveEventPayload::AssistantMessageStarted { message_id }) => {
            *guard = Some((message_id.clone(), String::new(), String::new()));
        },
        EventPayload::Live(LiveEventPayload::AssistantTextDelta { delta, .. }) => {
            if let Some((_, text, _)) = guard.as_mut() {
                text.push_str(delta);
            }
        },
        EventPayload::Live(LiveEventPayload::ThinkingDelta { delta, .. }) => {
            if let Some((_, _, reasoning)) = guard.as_mut() {
                reasoning.push_str(delta);
            }
        },
        EventPayload::Durable(
            DurableEventPayload::AssistantMessageCompleted { .. }
            | DurableEventPayload::TurnCompleted { .. },
        ) => {
            *guard = None;
        },
        _ => {},
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{
            DurableEvent, LiveEvent, PersistedSystemPrompt, SessionStarted, StoredEvent,
            SystemPromptSource,
        },
        tool::SessionToolSelection,
        types::{SessionId, ToolCallId},
    };
    use astrcode_session_projection::replay;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;

    fn durable(session_id: SessionId, payload: DurableEventPayload) -> Arc<Event> {
        Arc::new(StoredEvent::new(1, DurableEvent::session(session_id, payload)).into())
    }

    fn live(session_id: SessionId, payload: LiveEventPayload) -> Arc<Event> {
        Arc::new(LiveEvent::session(session_id, payload).into())
    }

    fn turn_started(session_id: &SessionId) -> Arc<Event> {
        durable(session_id.clone(), DurableEventPayload::TurnStarted)
    }

    fn session_read_model(session_id: &str) -> SessionReadModel {
        let session_id = SessionId::new(session_id);
        replay(
            session_id.clone(),
            &[StoredEvent::new(
                0,
                DurableEvent::session(
                    session_id,
                    DurableEventPayload::SessionStarted(SessionStarted {
                        working_dir: "/workspace".into(),
                        model_id: "model".into(),
                        parent: None,
                        tool_selection: SessionToolSelection::default(),
                        source_extension: None,
                        initial_system_prompt: PersistedSystemPrompt {
                            text: "system".into(),
                            fingerprint: "fingerprint".into(),
                            extra_system_prompt: None,
                            source: SystemPromptSource::Native,
                        },
                    }),
                ),
            )],
        )
        .expect("session read model")
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
    async fn all_notifications_include_events_and_global_notifications() {
        let bus = ServerEventBus::new();
        let session_id = SessionId::new("session-1");
        let mut all_rx = bus.subscribe_all_notifications();

        bus.publish_event(turn_started(&session_id));
        bus.send_notification(ClientNotification::ExtensionRegistryChanged);

        let notification = all_rx.recv().await.expect("event notification");
        match notification {
            ClientNotification::Event(event) => assert_eq!(event.session_id, session_id),
            other => panic!("expected event notification, got {other:?}"),
        }
        assert!(matches!(
            all_rx.recv().await,
            Ok(ClientNotification::ExtensionRegistryChanged)
        ));
    }

    #[tokio::test]
    async fn semantic_notifications_preserve_transport_wire_shape() {
        let bus = ServerEventBus::new();
        let mut rx = bus.subscribe_global_notifications();

        bus.send_session_resumed(&session_read_model("session-1"));
        bus.send_status_item_update("mode".into(), "plan".into());
        bus.send_extension_command_result("review".into(), "done".into(), false);

        let resumed = serde_json::to_value(rx.recv().await.expect("session resumed")).unwrap();
        assert_eq!(resumed["event"], "session_resumed");
        assert_eq!(resumed["data"]["session_id"], "session-1");
        assert_eq!(resumed["data"]["snapshot"]["session_id"], "session-1");
        assert_eq!(resumed["data"]["snapshot"]["working_dir"], "/workspace");

        let status = serde_json::to_value(rx.recv().await.expect("status update")).unwrap();
        assert_eq!(
            status,
            serde_json::json!({
                "event": "status_item_update",
                "data": {
                    "id": "mode",
                    "text": "plan"
                }
            })
        );

        let result = serde_json::to_value(rx.recv().await.expect("command result")).unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "event": "extension_command_result",
                "data": {
                    "command_name": "review",
                    "content": "done",
                    "is_error": false
                }
            })
        );
    }

    #[tokio::test]
    async fn conversation_routes_filter_agent_noise_and_forward_progress() {
        let bus = ServerEventBus::new();
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut parent_rx = bus.subscribe_conversation_events(&parent);

        bus.publish_event(durable(
            parent.clone(),
            DurableEventPayload::AgentSessionSpawned {
                child_session_id: child.clone(),
                agent_name: "explore".into(),
                task: "inspect".into(),
                tool_selection: None,
                tool_call_id: ToolCallId::new("agent-call"),
            },
        ));
        assert!(matches!(
            parent_rx.recv().await,
            Ok(event) if event.session_id == parent
        ));

        for _ in 0..=ServerEventBus::EVENT_CHANNEL_CAPACITY {
            bus.publish_event(live(
                child.clone(),
                LiveEventPayload::AssistantTextDelta {
                    message_id: "message-1".into(),
                    delta: "token".into(),
                },
            ));
        }
        assert!(matches!(parent_rx.try_recv(), Err(TryRecvError::Empty)));

        bus.publish_event(live(
            child.clone(),
            LiveEventPayload::ToolCallStarted {
                call_id: "tool-1".into(),
                tool_name: "read".into(),
            },
        ));
        assert!(matches!(
            parent_rx.recv().await,
            Ok(event) if event.session_id == child
        ));
    }
}
