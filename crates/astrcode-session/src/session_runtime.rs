use std::{collections::HashMap, sync::Arc, time::Duration};

use astrcode_core::{
    permission::ApprovalDecision,
    tool::FileObservationStore,
    types::{SessionId, ToolCallId},
};
use astrcode_storage::SessionStore;
use parking_lot::Mutex;
use tokio::sync::{OnceCell, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    compaction::CompactCircuitBreaker,
    permission::ApprovalHistoryStore,
    session_event_sink::{SessionEventObserver, SessionEventSink},
    session_tools::SessionToolCache,
    tool_exec::InMemoryFileObservationStore,
};

pub struct PendingApprovalRegistration<'a> {
    runtime: &'a ApprovalRuntime,
    call_id: ToolCallId,
    registration: Arc<()>,
}

impl Drop for PendingApprovalRegistration<'_> {
    fn drop(&mut self) {
        self.runtime
            .remove_pending(&self.call_id, &self.registration);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolApprovalRegistrationError {
    #[error("approval already pending for call_id {call_id}")]
    AlreadyPending { call_id: ToolCallId },
}

/// 解析挂起工具审批时的错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolApprovalResolveError {
    #[error("no pending approval for call_id {call_id}")]
    NotPending { call_id: ToolCallId },
    #[error("approval receiver dropped for call_id {call_id}")]
    ReceiverDropped { call_id: ToolCallId },
}

/// 执行工具所需的进程内资源。
struct ToolResources {
    file_observation_store: Arc<dyn FileObservationStore>,
    registry_snapshots: SessionToolCache,
}

struct PendingApproval {
    registration: Arc<()>,
    sender: oneshot::Sender<ApprovalDecision>,
}

struct ApprovalRuntime {
    history: Arc<ApprovalHistoryStore>,
    pending: Mutex<HashMap<ToolCallId, PendingApproval>>,
}

impl ApprovalRuntime {
    fn new() -> Self {
        Self {
            history: Arc::new(ApprovalHistoryStore::default()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn register(
        &self,
        call_id: ToolCallId,
        sender: oneshot::Sender<ApprovalDecision>,
    ) -> Result<PendingApprovalRegistration<'_>, ToolApprovalRegistrationError> {
        let mut pending = self.pending.lock();
        if pending.contains_key(&call_id) {
            return Err(ToolApprovalRegistrationError::AlreadyPending { call_id });
        }

        let registration = Arc::new(());
        pending.insert(
            call_id.clone(),
            PendingApproval {
                registration: Arc::clone(&registration),
                sender,
            },
        );
        Ok(PendingApprovalRegistration {
            runtime: self,
            call_id,
            registration,
        })
    }

    fn remove_pending(&self, call_id: &ToolCallId, registration: &Arc<()>) {
        let mut pending = self.pending.lock();
        let matches_registration = pending
            .get(call_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.registration, registration));
        if matches_registration {
            pending.remove(call_id);
        }
    }

    fn resolve(
        &self,
        call_id: &ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<(), ToolApprovalResolveError> {
        let pending =
            self.pending
                .lock()
                .remove(call_id)
                .ok_or(ToolApprovalResolveError::NotPending {
                    call_id: call_id.clone(),
                })?;
        pending
            .sender
            .send(decision)
            .map_err(|_| ToolApprovalResolveError::ReceiverDropped {
                call_id: call_id.clone(),
            })
    }

    fn cancel_pending(&self) {
        self.pending.lock().clear();
    }
}

/// 单个 session 在当前进程内持有的瞬态状态。
///
/// 持久化事实仍以 [`SessionStore`] 为准；同一 sid 的并存 [`crate::Session`] 通过
/// [`crate::SessionResourceStore`] 共享这里的工具缓存、审批状态和事件排序 lane。
pub struct SessionRuntimeState {
    session_id: SessionId,
    store: Arc<dyn SessionStore>,
    event_sink: Arc<SessionEventSink>,
    tools: ToolResources,
    approvals: ApprovalRuntime,
    compact_circuit_breaker: Mutex<CompactCircuitBreaker>,
    creation: Mutex<SessionCreationState>,
    lifecycle_initialized: OnceCell<()>,
}

#[derive(Default)]
enum SessionCreationState {
    #[default]
    Ready,
    Pending(Arc<PendingSessionCreation>),
    Failed,
}

#[derive(Default)]
struct PendingSessionCreation(CancellationToken);

impl PendingSessionCreation {
    async fn wait(&self) {
        self.0.cancelled().await;
    }

    fn finish(&self) {
        self.0.cancel();
    }
}

/// Keeps concurrent openers behind the new-session initialization boundary.
#[must_use = "the creation guard must be held until initialization commits or is compensated"]
pub struct SessionCreationGuard {
    runtime: Arc<SessionRuntimeState>,
    pending: Arc<PendingSessionCreation>,
    committed: bool,
}

impl SessionCreationGuard {
    pub fn commit(mut self) {
        let mut creation = self.runtime.creation.lock();
        if matches!(
            &*creation,
            SessionCreationState::Pending(current) if Arc::ptr_eq(current, &self.pending)
        ) {
            *creation = SessionCreationState::Ready;
            drop(creation);
            self.pending.finish();
        }
        self.committed = true;
    }
}

impl Drop for SessionCreationGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut creation = self.runtime.creation.lock();
        if matches!(
            &*creation,
            SessionCreationState::Pending(current) if Arc::ptr_eq(current, &self.pending)
        ) {
            *creation = SessionCreationState::Failed;
            drop(creation);
            self.pending.finish();
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("session creation failed before lifecycle initialization committed")]
pub struct SessionCreationFailed;

impl SessionRuntimeState {
    pub fn new(session_id: SessionId, store: Arc<dyn SessionStore>) -> Self {
        let event_sink = Arc::new(SessionEventSink::new(Arc::new(IgnoreEvents)));
        Self::new_with_event_sink(session_id, store, event_sink)
    }

    pub fn new_with_event_sink(
        session_id: SessionId,
        store: Arc<dyn SessionStore>,
        event_sink: Arc<SessionEventSink>,
    ) -> Self {
        Self {
            session_id,
            store,
            event_sink,
            tools: ToolResources {
                file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
                registry_snapshots: SessionToolCache::new(),
            },
            approvals: ApprovalRuntime::new(),
            compact_circuit_breaker: Mutex::new(CompactCircuitBreaker::new(
                3,
                Duration::from_secs(60),
            )),
            creation: Mutex::new(SessionCreationState::Ready),
            lifecycle_initialized: OnceCell::new(),
        }
    }

    pub fn file_observation_store(&self) -> Arc<dyn FileObservationStore> {
        Arc::clone(&self.tools.file_observation_store)
    }

    pub(crate) fn tool_registry_cache(&self) -> &SessionToolCache {
        &self.tools.registry_snapshots
    }

    pub(crate) fn compact_circuit_breaker(&self) -> &Mutex<CompactCircuitBreaker> {
        &self.compact_circuit_breaker
    }

    pub(crate) fn configure_compact_circuit_breaker(&self, threshold: u32, cooldown: Duration) {
        self.compact_circuit_breaker
            .lock()
            .reconfigure(threshold, cooldown);
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }

    pub(crate) fn event_sink(&self) -> &SessionEventSink {
        &self.event_sink
    }

    pub(crate) fn event_sink_arc(&self) -> Arc<SessionEventSink> {
        Arc::clone(&self.event_sink)
    }

    pub fn approval_history(&self) -> Arc<ApprovalHistoryStore> {
        Arc::clone(&self.approvals.history)
    }

    /// Blocks [`Self::wait_for_creation`] until the returned guard commits or fails.
    pub fn begin_creation(self: &Arc<Self>) -> SessionCreationGuard {
        let mut creation = self.creation.lock();
        debug_assert!(
            matches!(*creation, SessionCreationState::Ready),
            "session creation must have one owner"
        );
        let pending = Arc::new(PendingSessionCreation::default());
        *creation = SessionCreationState::Pending(Arc::clone(&pending));
        SessionCreationGuard {
            runtime: Arc::clone(self),
            pending,
            committed: false,
        }
    }

    /// Waits only when this process is currently creating the session.
    pub async fn wait_for_creation(&self) -> Result<(), SessionCreationFailed> {
        loop {
            let pending = match &*self.creation.lock() {
                SessionCreationState::Ready => return Ok(()),
                SessionCreationState::Failed => return Err(SessionCreationFailed),
                SessionCreationState::Pending(pending) => Arc::clone(pending),
            };
            pending.wait().await;
        }
    }

    pub(crate) async fn ensure_lifecycle_initialized<E, F, Fut>(
        &self,
        initialize: F,
    ) -> Result<(), E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
    {
        self.lifecycle_initialized
            .get_or_try_init(initialize)
            .await
            .map(|_| ())
    }

    pub fn register_pending_approval(
        &self,
        call_id: ToolCallId,
        sender: oneshot::Sender<ApprovalDecision>,
    ) -> Result<PendingApprovalRegistration<'_>, ToolApprovalRegistrationError> {
        self.approvals.register(call_id, sender)
    }

    pub fn resolve_tool_approval(
        &self,
        call_id: &ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<(), ToolApprovalResolveError> {
        self.approvals.resolve(call_id, decision)
    }

    pub fn cancel_pending_approvals(&self) {
        self.approvals.cancel_pending();
    }
}

struct IgnoreEvents;

impl SessionEventObserver for IgnoreEvents {
    fn publish(&self, _event: Arc<astrcode_core::event::Event>) {}
}

#[cfg(test)]
mod tests {
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;

    fn runtime() -> SessionRuntimeState {
        let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
        SessionRuntimeState::new(SessionId::from("runtime-test"), store)
    }

    #[tokio::test]
    async fn pending_approval_registration_preserves_ownership_and_cleans_up() {
        let runtime = runtime();
        let call_id = ToolCallId::from("tool-1");

        let (first_tx, first_rx) = oneshot::channel();
        let first_guard = runtime
            .register_pending_approval(call_id.clone(), first_tx)
            .unwrap();

        assert!(matches!(
            runtime.register_pending_approval(call_id.clone(), oneshot::channel().0),
            Err(ToolApprovalRegistrationError::AlreadyPending { .. })
        ));

        runtime
            .resolve_tool_approval(&call_id, ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(first_rx.await.unwrap(), ApprovalDecision::AllowOnce);

        let (second_tx, second_rx) = oneshot::channel();
        let second_guard = runtime
            .register_pending_approval(call_id.clone(), second_tx)
            .unwrap();
        drop(first_guard);
        runtime
            .resolve_tool_approval(&call_id, ApprovalDecision::DenyOnce)
            .unwrap();
        assert_eq!(second_rx.await.unwrap(), ApprovalDecision::DenyOnce);
        drop(second_guard);

        let cleanup_call_id = ToolCallId::from("tool-2");
        let cleanup_guard = runtime
            .register_pending_approval(cleanup_call_id.clone(), oneshot::channel().0)
            .unwrap();
        drop(cleanup_guard);

        assert!(matches!(
            runtime.resolve_tool_approval(&cleanup_call_id, ApprovalDecision::DenyOnce),
            Err(ToolApprovalResolveError::NotPending { .. })
        ));
    }
}
