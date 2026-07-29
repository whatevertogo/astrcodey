use std::{collections::HashMap, sync::Arc, time::Duration};

use astrcode_core::{
    event::Event,
    llm::LlmProvider,
    permission::ApprovalDecision,
    tool::FileObservationStore,
    types::{SessionId, ToolCallId},
};
use astrcode_storage::{SessionEventJournal, SessionStore};
use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};

use crate::{
    compaction::CompactCircuitBreaker,
    event_publisher::{SessionEventPublishError, SessionEventPublisher},
    permission::ApprovalHistoryStore,
    session_tools::SessionToolCache,
    tool_exec::InMemoryFileObservationStore,
};

const SESSION_EVENT_CAPACITY: usize = 1024;

pub struct PendingApprovalRegistration<'a> {
    runtime: &'a SessionRuntimeState,
    call_id: ToolCallId,
}

impl Drop for PendingApprovalRegistration<'_> {
    fn drop(&mut self) {
        self.runtime.remove_pending_approval(&self.call_id);
    }
}

/// 解析挂起工具审批时的错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolApprovalResolveError {
    #[error("no pending approval for call_id {call_id}")]
    NotPending { call_id: ToolCallId },
    #[error("approval receiver dropped for call_id {call_id}")]
    ReceiverDropped { call_id: ToolCallId },
}

/// 当前 session 使用的模型绑定；一次替换同时切换 provider 与模型标识。
#[derive(Clone)]
pub struct SessionModelBinding {
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) small_llm: Arc<dyn LlmProvider>,
    pub(crate) model_id: String,
}

impl SessionModelBinding {
    pub fn llm(&self) -> &dyn LlmProvider {
        self.llm.as_ref()
    }

    pub fn small_llm(&self) -> &dyn LlmProvider {
        self.small_llm.as_ref()
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// 执行工具所需的进程内资源。
struct ToolResources {
    file_observation_store: Arc<dyn FileObservationStore>,
    registry_snapshots: SessionToolCache,
}

/// 单个 session 在当前进程内持有的瞬态状态。
///
/// 这里的状态随 session 生命周期存在，但不属于可持久化事实；此类型仅组合按职责
/// 拆分的运行态组件，并作为 session 调用方的稳定门面。
///
/// `event_out` 故意放在 `SessionRuntimeState` 而非 `Session`：同一 sid 多次
/// `Session::open` 会得到多个 `Session` 实例（廉价的 store handle clone），
/// 但所有实例必须共享同一份 `SessionRuntimeState`（含 broadcast sender）才能让所有订阅者
/// 看到全部事件。SessionRuntimeRegistry / SessionManager 保证 per-sid 唯一。
///
/// 注意：直接通过 `Session::create_with_params` 绕过 `SessionRuntimeRegistry` 时，runtime
/// 完全由调用者提供；同一 sid 若使用不同 runtime，实例会彼此隔离，订阅者也不会跨实例可见。
/// `spawn_child` 会有意创建新 runtime，SessionManager 则始终走 registry 路径。
pub struct SessionRuntimeState {
    store: Arc<dyn SessionStore>,
    model: Mutex<SessionModelBinding>,
    tools: ToolResources,
    /// 熔断器需要 &mut self 的状态转换（Open→HalfOpen）。
    compact_circuit_breaker: Mutex<CompactCircuitBreaker>,
    /// 本 session 事件的 fan-out 通道。同一 sid 下所有 Session 实例共享这份 sender，
    /// 通过 SessionRuntimeState 的 Arc 共享保证订阅一致性。
    event_out: broadcast::Sender<Arc<Event>>,
    /// 同一 session 的 durable/live 事件共享一个有序发布管线。
    event_publisher: SessionEventPublisher,
    /// 会话级 AllowAlways / DenyAlways 审批记忆。
    approval_history: Arc<ApprovalHistoryStore>,
    /// 挂起中的工具审批（call_id → oneshot sender）。
    pending_approvals: Mutex<HashMap<ToolCallId, oneshot::Sender<ApprovalDecision>>>,
}

impl SessionRuntimeState {
    pub fn new(
        session_id: SessionId,
        store: Arc<dyn SessionStore>,
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        model_id: String,
    ) -> Self {
        let (event_out, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
        let journal: Arc<dyn SessionEventJournal> = store.clone();
        let event_publisher = SessionEventPublisher::start(session_id, journal, event_out.clone());
        Self {
            store,
            model: Mutex::new(SessionModelBinding {
                llm,
                small_llm,
                model_id,
            }),
            tools: ToolResources {
                file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
                registry_snapshots: SessionToolCache::new(),
            },
            compact_circuit_breaker: Mutex::new(CompactCircuitBreaker::new(
                3,
                Duration::from_secs(60),
            )),
            event_out,
            event_publisher,
            approval_history: Arc::new(ApprovalHistoryStore::default()),
            pending_approvals: Mutex::new(HashMap::new()),
        }
    }

    /// 返回 provider 与模型标识的一致快照。
    ///
    /// 需要同时读取 `llm` / `small_llm` / `model_id` 时请用此方法；
    /// 分别调用 [`Self::llm`]、[`Self::small_llm`] 可能在替换间隙读到不一致组合。
    pub fn model_binding(&self) -> SessionModelBinding {
        self.model.lock().clone()
    }

    pub fn model_id(&self) -> String {
        self.model.lock().model_id.clone()
    }

    pub fn replace_model_binding(
        &self,
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        model_id: String,
    ) {
        *self.model.lock() = SessionModelBinding {
            llm,
            small_llm,
            model_id,
        };
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

    /// 订阅本 session 的事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.event_out.subscribe()
    }

    pub fn session_id(&self) -> &SessionId {
        self.event_publisher.session_id()
    }

    pub(crate) fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }

    pub(crate) fn event_publisher(&self) -> &SessionEventPublisher {
        &self.event_publisher
    }

    pub async fn sync_durable_events(&self) -> Result<(), SessionEventPublishError> {
        self.event_publisher.sync_durable().await
    }

    pub async fn shutdown_event_publisher(&self) -> Result<(), SessionEventPublishError> {
        self.event_publisher.shutdown().await
    }

    pub fn approval_history(&self) -> Arc<ApprovalHistoryStore> {
        Arc::clone(&self.approval_history)
    }

    pub fn register_pending_approval(
        &self,
        call_id: ToolCallId,
        sender: oneshot::Sender<ApprovalDecision>,
    ) -> PendingApprovalRegistration<'_> {
        self.pending_approvals
            .lock()
            .insert(call_id.clone(), sender);
        PendingApprovalRegistration {
            runtime: self,
            call_id,
        }
    }

    fn remove_pending_approval(&self, call_id: &ToolCallId) {
        self.pending_approvals.lock().remove(call_id);
    }

    pub fn resolve_tool_approval(
        &self,
        call_id: &ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<(), ToolApprovalResolveError> {
        let sender = self.pending_approvals.lock().remove(call_id).ok_or(
            ToolApprovalResolveError::NotPending {
                call_id: call_id.clone(),
            },
        )?;
        sender
            .send(decision)
            .map_err(|_| ToolApprovalResolveError::ReceiverDropped {
                call_id: call_id.clone(),
            })
    }

    pub fn cancel_pending_approvals(&self) {
        self.pending_approvals.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    };

    use astrcode_core::{
        llm::{LlmError, LlmEvent, LlmMessage, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio::sync::mpsc;

    use super::*;

    struct TaggedLlm {
        tag: usize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for TaggedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            unreachable!("binding test does not generate completions")
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: self.tag,
                max_output_tokens: self.tag,
            }
        }
    }

    fn provider(tag: usize) -> Arc<dyn LlmProvider> {
        Arc::new(TaggedLlm { tag })
    }

    fn runtime(tag: usize) -> SessionRuntimeState {
        let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
        SessionRuntimeState::new(
            SessionId::from("runtime-test"),
            store,
            provider(tag),
            provider(tag + 1000),
            tag.to_string(),
        )
    }

    fn assert_consistent_binding(binding: &SessionModelBinding) {
        let tag: usize = binding.model_id().parse().unwrap();
        assert_eq!(binding.llm().model_limits().max_input_tokens, tag);
        assert_eq!(
            binding.small_llm().model_limits().max_input_tokens,
            tag + 1000
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn model_binding_replacement_is_atomic() {
        let runtime = Arc::new(runtime(1));
        let running = Arc::new(AtomicBool::new(true));
        let start = Arc::new(Barrier::new(2));

        let reader_runtime = Arc::clone(&runtime);
        let reader_running = Arc::clone(&running);
        let reader_start = Arc::clone(&start);
        let reader = std::thread::spawn(move || {
            reader_start.wait();
            loop {
                assert_consistent_binding(&reader_runtime.model_binding());
                if !reader_running.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        start.wait();
        for tag in 2..10_000 {
            runtime.replace_model_binding(provider(tag), provider(tag + 1000), tag.to_string());
        }
        running.store(false, Ordering::Relaxed);
        reader.join().unwrap();
        assert_consistent_binding(&runtime.model_binding());
    }

    #[tokio::test]
    async fn pending_approval_registration_cleans_up_on_drop() {
        let runtime = runtime(1);
        let (tx, _rx) = oneshot::channel();
        let call_id = ToolCallId::from("tool-1");

        let guard = runtime.register_pending_approval(call_id.clone(), tx);
        drop(guard);

        assert!(matches!(
            runtime.resolve_tool_approval(&call_id, ApprovalDecision::DenyOnce),
            Err(ToolApprovalResolveError::NotPending { .. })
        ));
    }

    #[tokio::test]
    async fn pending_approval_resolve_then_guard_drop_is_noop() {
        let runtime = runtime(1);
        let (tx, _rx) = oneshot::channel();
        let call_id = ToolCallId::from("tool-1");

        let guard = runtime.register_pending_approval(call_id.clone(), tx);
        runtime
            .resolve_tool_approval(&call_id, ApprovalDecision::DenyOnce)
            .unwrap();
        drop(guard);

        assert!(matches!(
            runtime.resolve_tool_approval(&call_id, ApprovalDecision::DenyOnce),
            Err(ToolApprovalResolveError::NotPending { .. })
        ));
    }
}
