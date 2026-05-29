//! 客户端事件 fan-out 与 internal session event reactor。
//!
//! - **Client Event Bus**：`Session` live 事件 → `ClientNotification::Event`，维护 streaming 快照。
//! - **Internal Event Reactor**：background task 完成等 server 编排副作用（与 fan-out 解耦）。

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

use crate::turn_scheduler::{InputDelivery, TurnScheduler};

pub(crate) struct StreamingSnapshot {
    pub message_id: String,
    pub text: String,
    pub reasoning_content: Option<String>,
}

type StreamingState = parking_lot::Mutex<Option<(MessageId, String, String)>>;

pub struct ServerEventBus {
    tx: Arc<EventFanout<ClientNotification>>,
    attached: Mutex<HashSet<SessionId>>,
    reactor_attached: Mutex<HashSet<SessionId>>,
    streaming: Mutex<HashMap<SessionId, Arc<StreamingState>>>,
    scheduler: Arc<TurnScheduler>,
}

impl ServerEventBus {
    pub fn new(tx: Arc<EventFanout<ClientNotification>>, scheduler: Arc<TurnScheduler>) -> Self {
        Self {
            tx,
            attached: Mutex::new(HashSet::new()),
            reactor_attached: Mutex::new(HashSet::new()),
            streaming: Mutex::new(HashMap::new()),
            scheduler,
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
        self.attach_internal_reactor(session);
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

    fn attach_internal_reactor(&self, session: &Session) {
        let session_id = session.id().clone();
        if !self.reactor_attached.lock().insert(session_id.clone()) {
            return;
        }
        let mut rx = session.subscribe();
        let scheduler = Arc::clone(&self.scheduler);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if !matches!(event.payload, EventPayload::BackgroundTaskCompleted { .. }) {
                    continue;
                }
                let scheduler = Arc::clone(&scheduler);
                let sid = session_id.clone();
                tokio::spawn(async move {
                    let marker =
                        r#"<system type="background_completed" source="task">"#.to_string();
                    if let Err(e) = scheduler
                        .deliver_input(sid.clone(), marker, InputDelivery::InjectIfRunningElseStart)
                        .await
                    {
                        tracing::warn!(
                            session_id = %sid,
                            error = %e,
                            "failed to notify background completion (step path)"
                        );
                    }
                });
            }
        });
    }

    pub fn detach(&self, session_id: &SessionId) {
        self.attached.lock().remove(session_id);
        self.reactor_attached.lock().remove(session_id);
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
