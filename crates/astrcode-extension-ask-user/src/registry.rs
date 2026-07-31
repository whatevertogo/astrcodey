use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_extension_sdk::extension::{ExtensionError, ExtensionEventSink};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::oneshot;

use crate::model::PendingQuestion;

pub const PENDING_EVENT_TYPE: &str = "ask_user.pending";
pub const RESOLVED_EVENT_TYPE: &str = "ask_user.resolved";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Answered(HashMap<String, String>),
    Rejected,
    TimedOut,
    TurnCancelled,
    SessionShutdown,
    ExtensionShutdown,
}

impl Resolution {
    fn event_name(&self) -> &'static str {
        match self {
            Self::Answered(_) => "answered",
            Self::Rejected => "rejected",
            Self::TimedOut => "timed_out",
            Self::TurnCancelled => "turn_cancelled",
            Self::SessionShutdown => "session_shutdown",
            Self::ExtensionShutdown => "extension_shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound,
    AlreadyResolved,
    InvalidAnswers(String),
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PendingKey {
    session_id: String,
    call_id: String,
}

impl PendingKey {
    fn new(session_id: impl Into<String>, call_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            call_id: call_id.into(),
        }
    }
}

struct PendingEntry {
    question: PendingQuestion,
    sender: oneshot::Sender<Resolution>,
    events: Arc<dyn ExtensionEventSink>,
    registration: Arc<()>,
}

#[derive(Default)]
struct RegistryState {
    pending: HashMap<PendingKey, PendingEntry>,
    resolved: HashSet<PendingKey>,
}

#[derive(Default)]
pub struct PendingRegistry {
    state: Mutex<RegistryState>,
}

impl PendingRegistry {
    pub fn register(
        self: &Arc<Self>,
        question: PendingQuestion,
        events: Arc<dyn ExtensionEventSink>,
    ) -> Result<(oneshot::Receiver<Resolution>, PendingGuard), ExtensionError> {
        let key = PendingKey::new(&question.session_id, &question.call_id);
        let (sender, receiver) = oneshot::channel();
        let registration = Arc::new(());
        let mut state = self.state.lock();
        if state.pending.contains_key(&key) {
            return Err(ExtensionError::Internal(
                "askUser call id was already used".into(),
            ));
        }
        state.resolved.remove(&key);
        state.pending.insert(
            key.clone(),
            PendingEntry {
                question: question.clone(),
                sender,
                events: Arc::clone(&events),
                registration: Arc::clone(&registration),
            },
        );
        drop(state);

        if let Err(error) = events.emit(
            PENDING_EVENT_TYPE,
            1,
            serde_json::to_value(&question).unwrap_or_else(|_| json!({})),
        ) {
            self.abandon_registered(&key, &registration);
            return Err(error);
        }

        Ok((
            receiver,
            PendingGuard {
                registry: Arc::clone(self),
                key: Some(key),
                registration,
            },
        ))
    }

    pub fn list(&self, session_id: &str) -> Vec<PendingQuestion> {
        let mut questions = self
            .state
            .lock()
            .pending
            .iter()
            .filter(|(key, _)| key.session_id == session_id)
            .map(|(_, entry)| entry.question.clone())
            .collect::<Vec<_>>();
        questions.sort_by(|left, right| left.call_id.cmp(&right.call_id));
        questions
    }

    pub fn answer(
        &self,
        session_id: &str,
        call_id: &str,
        answers: HashMap<String, String>,
    ) -> Result<(), ResolveError> {
        let key = PendingKey::new(session_id, call_id);
        {
            let state = self.state.lock();
            let Some(entry) = state.pending.get(&key) else {
                return Err(if state.resolved.contains(&key) {
                    ResolveError::AlreadyResolved
                } else {
                    ResolveError::NotFound
                });
            };
            entry
                .question
                .validate_answers(&answers)
                .map_err(ResolveError::InvalidAnswers)?;
        }
        self.resolve(&key, Resolution::Answered(answers))
    }

    pub fn reject(&self, session_id: &str, call_id: &str) -> Result<(), ResolveError> {
        self.resolve(&PendingKey::new(session_id, call_id), Resolution::Rejected)
    }

    pub fn timeout(&self, session_id: &str, call_id: &str) -> Result<(), ResolveError> {
        self.resolve(&PendingKey::new(session_id, call_id), Resolution::TimedOut)
    }

    pub fn shutdown_session(&self, session_id: &str) {
        let entries = {
            let mut state = self.state.lock();
            state.resolved.retain(|key| key.session_id != session_id);
            let keys = state
                .pending
                .keys()
                .filter(|key| key.session_id == session_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.pending.remove(&key).map(|entry| (key, entry)))
                .collect::<Vec<_>>()
        };
        for (key, entry) in entries {
            finish_entry(key, entry, Resolution::SessionShutdown);
        }
    }

    pub fn shutdown_extension(&self) {
        let entries = {
            let mut state = self.state.lock();
            state.resolved.clear();
            state.pending.drain().collect::<Vec<_>>()
        };
        for (key, entry) in entries {
            finish_entry(key, entry, Resolution::ExtensionShutdown);
        }
    }

    fn resolve(&self, key: &PendingKey, resolution: Resolution) -> Result<(), ResolveError> {
        let entry = {
            let mut state = self.state.lock();
            let Some(entry) = state.pending.remove(key) else {
                return Err(if state.resolved.contains(key) {
                    ResolveError::AlreadyResolved
                } else {
                    ResolveError::NotFound
                });
            };
            state.resolved.insert(key.clone());
            entry
        };
        finish_entry(key.clone(), entry, resolution);
        Ok(())
    }

    fn abandon_registered(&self, key: &PendingKey, registration: &Arc<()>) {
        let mut state = self.state.lock();
        let Some(entry) = state.pending.get(key) else {
            return;
        };
        if Arc::ptr_eq(&entry.registration, registration) {
            state.pending.remove(key);
        }
    }

    fn resolve_registered(&self, key: &PendingKey, registration: &Arc<()>, resolution: Resolution) {
        let entry = {
            let mut state = self.state.lock();
            let Some(entry) = state.pending.get(key) else {
                return;
            };
            if !Arc::ptr_eq(&entry.registration, registration) {
                return;
            }
            let Some(entry) = state.pending.remove(key) else {
                return;
            };
            state.resolved.insert(key.clone());
            entry
        };
        finish_entry(key.clone(), entry, resolution);
    }
}

fn finish_entry(key: PendingKey, entry: PendingEntry, resolution: Resolution) {
    let event_name = resolution.event_name();
    if entry.sender.send(resolution).is_err() {
        tracing::debug!(
            session_id = %key.session_id,
            call_id = %key.call_id,
            "ask-user receiver closed before resolution"
        );
    }
    if let Err(error) = entry.events.emit(
        RESOLVED_EVENT_TYPE,
        1,
        json!({
            "sessionId": key.session_id,
            "callId": key.call_id,
            "resolution": event_name,
        }),
    ) {
        tracing::warn!(%error, "failed to emit ask-user resolved event");
    }
}

pub struct PendingGuard {
    registry: Arc<PendingRegistry>,
    key: Option<PendingKey>,
    registration: Arc<()>,
}

impl PendingGuard {
    pub fn disarm(&mut self) {
        self.key = None;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        self.registry
            .resolve_registered(&key, &self.registration, Resolution::TurnCancelled);
    }
}
