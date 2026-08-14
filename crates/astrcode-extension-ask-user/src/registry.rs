use std::{collections::HashMap, sync::Arc};

use astrcode_extension_sdk::{
    builder::custom_event,
    extension::{CustomEventDeclaration, CustomEventDelivery, CustomEventEmitter, ExtensionError},
};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::oneshot;

use crate::model::PendingQuestion;

pub(crate) const PENDING_EVENT_TYPE: &str = "ask_user.pending";
pub(crate) const RESOLVED_EVENT_TYPE: &str = "ask_user.resolved";

pub(crate) fn custom_event_declarations() -> [CustomEventDeclaration; 2] {
    [
        custom_event(PENDING_EVENT_TYPE)
            .delivery(CustomEventDelivery::GlobalLive)
            .build(),
        custom_event(RESOLVED_EVENT_TYPE)
            .delivery(CustomEventDelivery::GlobalLive)
            .build(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    Answered(HashMap<String, String>),
    AutoAnswered(HashMap<String, String>),
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
            Self::AutoAnswered(_) => "auto_answered",
            Self::Rejected => "rejected",
            Self::TimedOut => "timed_out",
            Self::TurnCancelled => "turn_cancelled",
            Self::SessionShutdown => "session_shutdown",
            Self::ExtensionShutdown => "extension_shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    NotFound,
    AlreadyResolved,
    InvalidAnswers(String),
    /// 问题没有推荐选项，无法自动选择。
    NoRecommended,
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
    events: CustomEventEmitter,
    registration: Arc<()>,
}

#[derive(Default)]
struct RegistryState {
    pending: HashMap<PendingKey, PendingEntry>,
    resolved: HashMap<PendingKey, Arc<()>>,
}

#[derive(Default)]
pub(crate) struct PendingRegistry {
    state: Mutex<RegistryState>,
}

impl PendingRegistry {
    pub(crate) fn register(
        self: &Arc<Self>,
        question: PendingQuestion,
        events: CustomEventEmitter,
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
                events: events.clone(),
                registration: Arc::clone(&registration),
            },
        );
        drop(state);

        if let Err(error) = events.try_emit(PENDING_EVENT_TYPE, &question) {
            self.abandon_registered(&key, &registration);
            return Err(ExtensionError::Internal(error.to_string()));
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

    pub(crate) fn list(&self, session_id: &str) -> Vec<PendingQuestion> {
        let mut questions = self
            .state
            .lock()
            .pending
            .iter()
            .filter(|(key, _)| key.session_id == session_id)
            .map(|(_, entry)| entry.question.with_current_server_time())
            .collect::<Vec<_>>();
        questions.sort_by(|left, right| left.call_id.cmp(&right.call_id));
        questions
    }

    pub(crate) fn list_all(&self) -> Vec<PendingQuestion> {
        let mut questions = self
            .state
            .lock()
            .pending
            .values()
            .map(|entry| entry.question.with_current_server_time())
            .collect::<Vec<_>>();
        questions.sort_by(|left, right| {
            left.session_id
                .cmp(&right.session_id)
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        questions
    }

    pub(crate) fn answer(
        &self,
        session_id: &str,
        call_id: &str,
        answers: HashMap<String, String>,
    ) -> Result<(), ResolveError> {
        let key = PendingKey::new(session_id, call_id);
        {
            let state = self.state.lock();
            let Some(entry) = state.pending.get(&key) else {
                return Err(if state.resolved.contains_key(&key) {
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

    pub(crate) fn reject(&self, session_id: &str, call_id: &str) -> Result<(), ResolveError> {
        self.resolve(&PendingKey::new(session_id, call_id), Resolution::Rejected)
    }

    /// 用户超时未响应时自动选择推荐选项。所有问题都必须有推荐选项，
    /// 否则返回 [`ResolveError::NoRecommended`] 且不改变任何状态。
    pub(crate) fn auto_select_recommended(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<(), ResolveError> {
        let key = PendingKey::new(session_id, call_id);
        let question = {
            let state = self.state.lock();
            let Some(entry) = state.pending.get(&key) else {
                return Err(if state.resolved.contains_key(&key) {
                    ResolveError::AlreadyResolved
                } else {
                    ResolveError::NotFound
                });
            };
            entry.question.clone()
        };
        let Some(answers) = question.auto_recommended_answers() else {
            return Err(ResolveError::NoRecommended);
        };
        self.resolve(&key, Resolution::AutoAnswered(answers))
    }

    pub(crate) fn timeout(&self, session_id: &str, call_id: &str) -> Result<(), ResolveError> {
        self.resolve(&PendingKey::new(session_id, call_id), Resolution::TimedOut)
    }

    pub(crate) fn shutdown_session(&self, session_id: &str) {
        let entries = {
            let mut state = self.state.lock();
            state.resolved.retain(|key, _| key.session_id != session_id);
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

    pub(crate) fn shutdown_extension(&self) {
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
                return Err(if state.resolved.contains_key(key) {
                    ResolveError::AlreadyResolved
                } else {
                    ResolveError::NotFound
                });
            };
            state
                .resolved
                .insert(key.clone(), Arc::clone(&entry.registration));
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
            state
                .resolved
                .insert(key.clone(), Arc::clone(&entry.registration));
            entry
        };
        finish_entry(key.clone(), entry, resolution);
    }

    fn forget_resolved(&self, key: &PendingKey, registration: &Arc<()>) {
        let mut state = self.state.lock();
        if state
            .resolved
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, registration))
        {
            state.resolved.remove(key);
        }
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
    if let Err(error) = entry.events.try_emit(
        RESOLVED_EVENT_TYPE,
        &json!({
            "sessionId": key.session_id,
            "callId": key.call_id,
            "resolution": event_name,
        }),
    ) {
        tracing::warn!(%error, "failed to emit ask-user resolved event");
    }
}

pub(crate) struct PendingGuard {
    registry: Arc<PendingRegistry>,
    key: Option<PendingKey>,
    registration: Arc<()>,
}

impl PendingGuard {
    pub(crate) fn disarm(&mut self) {
        if let Some(key) = self.key.take() {
            self.registry.forget_resolved(&key, &self.registration);
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        self.registry
            .resolve_registered(&key, &self.registration, Resolution::TurnCancelled);
        self.registry.forget_resolved(&key, &self.registration);
    }
}
