use std::{collections::HashMap, sync::Arc, time::Duration};

use astrcode_core::{
    event::{
        DurableEvent, EventDeliveryReceipt, EventPayload, EventPublisher, EventSendError,
        EventSender, LiveEvent,
    },
    permission::ApprovalDecision,
    tool::FileObservationStore,
    types::{SessionId, ToolCallId, TurnId},
};
use astrcode_storage::SessionStore;
use parking_lot::Mutex;
use tokio::sync::{OnceCell, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    permission::ApprovalHistoryStore,
    session_event_sink::{SessionEventObserver, SessionEventPublishError, SessionEventSink},
    session_tools::SessionToolCache,
    tool_exec::InMemoryFileObservationStore,
};

struct SessionScopedEventPublisher {
    session_id: SessionId,
    turn_id: Option<TurnId>,
    store: Arc<dyn SessionStore>,
    event_sink: Arc<SessionEventSink>,
}

#[async_trait::async_trait]
impl EventPublisher for SessionScopedEventPublisher {
    fn try_send(&self, payload: EventPayload) -> Result<(), EventSendError> {
        match payload {
            EventPayload::Live(payload) => self
                .event_sink
                .publish_live(
                    self.store.clone(),
                    LiveEvent::new(self.session_id.clone(), self.turn_id.clone(), payload),
                )
                .map_err(map_event_publish_error),
            // durable-in-try_send 与 turn 路径（`crate::turn_publish` 的
            // `TurnEventPublisher`，入队由 ingress worker 持久化）语义不同：session
            // 路径没有自己的 worker，直接拒绝；durable 须走 `send_confirmed`。
            EventPayload::Durable(_) => Err(EventSendError::PublishFailed(
                "durable custom events require async emit".into(),
            )),
        }
    }

    async fn send_confirmed(
        &self,
        payload: EventPayload,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        match payload {
            EventPayload::Durable(payload) => {
                let stored = self
                    .event_sink
                    .append(
                        self.store.clone(),
                        DurableEvent::new(self.session_id.clone(), self.turn_id.clone(), payload),
                    )
                    .await
                    .map_err(map_event_publish_error)?;
                Ok(EventDeliveryReceipt::Persisted {
                    event_id: stored.id.clone(),
                    seq: stored.seq,
                })
            },
            EventPayload::Live(payload) => {
                // 与 `session.rs` 的 `emit_live_required` 平行：`LivePublished`
                // 只表示事件已进入有序 lane，observer 派发是异步的。
                let event = LiveEvent::new(self.session_id.clone(), self.turn_id.clone(), payload);
                let event_id = event.id.clone();
                self.event_sink
                    .publish_live_required(self.store.clone(), event)
                    .await
                    .map_err(map_event_publish_error)?;
                Ok(EventDeliveryReceipt::LivePublished { event_id })
            },
        }
    }
}

// 与 `turn_publish.rs` ingress worker 的错误映射互指：这里的源错误
// `SessionEventPublishError` 带 Closed/Full 变体故可区分；turn 路径的 `TurnError`
// 无此信息，统一折叠为 `PublishFailed`，两处不宜强行收敛。
fn map_event_publish_error(error: SessionEventPublishError) -> EventSendError {
    match error {
        SessionEventPublishError::Closed => EventSendError::Closed,
        SessionEventPublishError::Full { .. } => EventSendError::Full,
        error => EventSendError::PublishFailed(error.to_string()),
    }
}

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
///
/// 创建窗口的兜底上限，与 SDK 侧生命周期 hook 的超时上限（120s，见
/// `astrcode-extension-sdk/src/runtime/peer.rs` 的 invoke 超时）对齐：hook 合法耗时
/// 可能接近该值，等待方超时会误报创建失败（但创建仍会完成，调用方可重试）。
/// 仅当单创建者不变式被违反、创建者 token 永远不会 finish 时才会真正挂起到此上限。
const CREATION_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

#[must_use = "the creation guard must be held until initialization commits or is compensated"]
pub struct SessionCreationGuard {
    runtime: Arc<SessionRuntimeState>,
    pending: Arc<PendingSessionCreation>,
    committed: bool,
}

impl SessionCreationGuard {
    /// 仅当 `creation` 仍指向本 guard 的 pending 状态时，将其替换为 `next` 并返回 true。
    ///
    /// commit 与 Drop 共用同一份"只有自己是当前创建者才推进状态"的判断：
    /// 若单创建者不变式被违反（见 [`SessionRuntimeState::begin_creation`]），后来的
    /// guard 覆盖了状态，先前的 guard 不应误改他人的状态。
    fn transition_pending(
        creation: &mut SessionCreationState,
        pending: &Arc<PendingSessionCreation>,
        next: SessionCreationState,
    ) -> bool {
        if matches!(
            &*creation,
            SessionCreationState::Pending(current) if Arc::ptr_eq(current, pending)
        ) {
            *creation = next;
            true
        } else {
            false
        }
    }

    pub fn commit(mut self) {
        let mut creation = self.runtime.creation.lock();
        if Self::transition_pending(&mut creation, &self.pending, SessionCreationState::Ready) {
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
        if Self::transition_pending(&mut creation, &self.pending, SessionCreationState::Failed) {
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

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Builds a session-scoped event ingress for runtime-owned asynchronous consumers.
    pub fn event_sender(&self, turn_id: Option<TurnId>) -> EventSender {
        Self::event_sender_from_parts(
            self.session_id.clone(),
            Arc::clone(&self.store),
            Arc::clone(&self.event_sink),
            turn_id,
        )
    }

    /// Builds event ingress without allocating a complete per-session runtime.
    #[doc(hidden)]
    pub fn event_sender_from_parts(
        session_id: SessionId,
        store: Arc<dyn SessionStore>,
        event_sink: Arc<SessionEventSink>,
        turn_id: Option<TurnId>,
    ) -> EventSender {
        EventSender::from_publisher(Arc::new(SessionScopedEventPublisher {
            session_id,
            turn_id,
            store,
            event_sink,
        }))
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
    ///
    /// 不变式：同一 runtime 同时只能有一个进行中的创建（单创建者）。该检查只有
    /// `debug_assert!`，release 下若被违反：后一个 guard 会覆盖前一个的 `Pending`
    /// 状态，前一个 guard 的 commit/Drop 因 `Arc::ptr_eq` 不匹配而不再 `finish()`
    /// 其 token——等待旧 token 的 [`Self::wait_for_creation`] 调用将永久挂起。
    /// 因此 `wait_for_creation` 对每次等待设置了超时兜底，超时按创建失败返回。
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
    ///
    /// 正常情况下等待时间很短（创建者 commit/Drop 即唤醒）；每次等待有超时兜底，
    /// 防止 [`Self::begin_creation`] 单创建者不变式被违反时永久挂起。
    pub async fn wait_for_creation(&self) -> Result<(), SessionCreationFailed> {
        let mut logged_creator: Option<usize> = None;
        loop {
            let pending = match &*self.creation.lock() {
                SessionCreationState::Ready => return Ok(()),
                SessionCreationState::Failed => return Err(SessionCreationFailed),
                SessionCreationState::Pending(pending) => Arc::clone(pending),
            };
            let creator = Arc::as_ptr(&pending) as usize;
            if logged_creator != Some(creator) {
                // 每个 pending 创建者只记一次，避免并发打开时的常规等待刷日志。
                logged_creator = Some(creator);
                tracing::debug!(
                    session_id = %self.session_id,
                    creator = %creator,
                    "waiting for in-flight session creation"
                );
            }
            match tokio::time::timeout(CREATION_WAIT_TIMEOUT, pending.wait()).await {
                Ok(()) => {},
                Err(_) => {
                    tracing::error!(
                        session_id = %self.session_id,
                        creator = %creator,
                        "timed out waiting for session creation; treating as failed \
                         (likely begin_creation single-owner invariant violation)"
                    );
                    return Err(SessionCreationFailed);
                },
            }
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

        let dropped_call_id = ToolCallId::from("tool-dropped");
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let _dropped_guard = runtime
            .register_pending_approval(dropped_call_id.clone(), dropped_tx)
            .unwrap();
        drop(dropped_rx);
        assert!(matches!(
            runtime.resolve_tool_approval(&dropped_call_id, ApprovalDecision::DenyOnce),
            Err(ToolApprovalResolveError::ReceiverDropped { .. })
        ));

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
