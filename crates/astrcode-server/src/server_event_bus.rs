//! 客户端事件 fan-out。
//!
//! `Session` live 事件 → `ClientNotification::Event`，维护 streaming 快照。

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

pub(crate) struct StreamingSnapshot {
    pub message_id: String,
    pub text: String,
    pub reasoning_content: Option<String>,
}

type StreamingState = parking_lot::Mutex<Option<(MessageId, String, String)>>;

pub struct ServerEventBus {
    tx: Arc<EventFanout<ClientNotification>>,
    attached: Mutex<HashSet<SessionId>>,
    streaming: Mutex<HashMap<SessionId, Arc<StreamingState>>>,
}

impl ServerEventBus {
    pub fn new(tx: Arc<EventFanout<ClientNotification>>) -> Self {
        Self {
            tx,
            attached: Mutex::new(HashSet::new()),
            streaming: Mutex::new(HashMap::new()),
        }
    }

    pub fn fanout(&self) -> &EventFanout<ClientNotification> {
        &self.tx
    }

    pub fn send_notification(&self, notification: ClientNotification) {
        self.tx.send(notification);
    }

    pub fn attach(&self, session: &Session) {
        self.attach_client_fanout(session);
    }

    fn attach_client_fanout(&self, session: &Session) {
        let session_id = session.id().clone();
        if !self.attached.lock().insert(session_id.clone()) {
            return;
        }
        let mut rx = session.subscribe();
        let tx = Arc::clone(&self.tx);
        let state = Arc::clone(
            self.streaming
                .lock()
                .entry(session_id)
                .or_insert_with(|| Arc::new(StreamingState::new(None))),
        );
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                update_streaming(&state, &event.payload);
                tx.send(ClientNotification::Event(event));
            }
        });
    }

    pub fn detach(&self, session_id: &SessionId) {
        self.attached.lock().remove(session_id);
        self.streaming.lock().remove(session_id);
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
