use std::{sync::Arc, time::Duration};

use astrcode_core::{
    event::Event, extension::ChildToolPolicy, llm::LlmProvider, tool::FileObservationStore,
    types::SessionId,
};
use astrcode_support::event_fanout::EventFanout;
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    background::BackgroundTaskManager,
    child_turn::{ChildTurnGuard, ChildTurnManager},
    compact_circuit_breaker::CompactCircuitBreaker,
    tool_exec::InMemoryFileObservationStore,
};

/// 单个 session 在当前进程内持有的瞬态状态。
/// TODO: 这里全是石山以后解决
///
/// 这里的状态随 session 生命周期存在，但不属于可持久化事实。
///
/// `event_out` 故意放在 `SessionRuntimeState` 而非 `Session`：同一 sid 多次
/// `Session::open` 会得到多个 `Session` 实例（廉价的 store handle clone），
/// 但所有实例必须共享同一份 `SessionRuntimeState`（含 EventFanout）才能让所有订阅者
/// 看到全部事件。SessionRuntimeRegistry / SessionManager 保证 per-sid 唯一。
///
/// 注意：直接通过 `Session::create_with_id` 而绕过 `SessionRuntimeRegistry` 创建的 session
/// 会得到独立 runtime，订阅者不会跨实例可见——这是给 `spawn_child` 这类「我就是要新 runtime」
/// 的场景用的。SessionManager 走 registry 路径。
/// Warning: 不推荐在这里放置非共享的进程内状态的内容
pub struct SessionRuntimeState {
    file_observation_store: Arc<dyn FileObservationStore>,
    tool_registry: Mutex<Arc<ToolRegistry>>,
    bg_tasks: Arc<Mutex<BackgroundTaskManager>>,
    /// 子 session 专用的额外 system prompt，由 SpawnRequest 注入。
    extra_system_prompt: Mutex<Option<String>>,
    /// 子 session 工具集策略，由 SpawnRequest 注入；父 session 始终为 `None`。
    ///
    /// `refresh_tools` 在每次重建工具表时读取此字段，保证子 session 的所有 turn
    /// 都看到一致的裁剪后工具集（含 resume 路径）。
    tool_policy: Mutex<Option<ChildToolPolicy>>,
    /// 熔断器需要 &mut self 的状态转换（Open→HalfOpen）。
    compact_circuit_breaker: Mutex<CompactCircuitBreaker>,
    /// 缓存的稳定前缀文本及其指纹（Identity → ProjectRules）。
    /// 首次构建后跨 turn 复用，compact 后清空触发全量重建。
    cached_stable_prefix: Mutex<Option<(String, String)>>,
    /// 本 session 事件的 fan-out 通道。同一 sid 下所有 Session 实例共享这份 sender，
    /// 通过 SessionRuntimeState 的 Arc 共享保证订阅一致性。
    event_out: Arc<EventFanout<Event>>,
    /// session-owned LLM provider。创建时从全局 caps 快照注入，
    /// turn 执行期间不会因其他 session 切换模型而改变。
    llm: Mutex<Arc<dyn LlmProvider>>,
    small_llm: Mutex<Arc<dyn LlmProvider>>,
    /// 当前 session 使用的模型 ID，与 llm provider 对应。
    model_id: Mutex<String>,
    /// 当前 session 派生的子 agent turn 管理器。
    child_turn_manager: ChildTurnManager,
    /// 子 agent 完成时向此通道发送 parent session_id。
    completed_tx: mpsc::UnboundedSender<SessionId>,
    completed_rx: Mutex<mpsc::UnboundedReceiver<SessionId>>,
}

impl SessionRuntimeState {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        model_id: String,
    ) -> Self {
        let event_out = Arc::new(EventFanout::new(1024));
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        Self {
            file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
            tool_registry: Mutex::new(Arc::new(ToolRegistry::new())),
            bg_tasks: Arc::new(Mutex::new(BackgroundTaskManager::new())),
            extra_system_prompt: Mutex::new(None),
            tool_policy: Mutex::new(None),
            compact_circuit_breaker: Mutex::new(CompactCircuitBreaker::new(
                3,
                Duration::from_secs(60),
            )),
            cached_stable_prefix: Mutex::new(None),
            event_out,
            llm: Mutex::new(llm),
            small_llm: Mutex::new(small_llm),
            model_id: Mutex::new(model_id),
            child_turn_manager: ChildTurnManager::new(),
            completed_tx,
            completed_rx: Mutex::new(completed_rx),
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.lock().clone()
    }

    pub fn set_llm(&self, provider: Arc<dyn LlmProvider>) {
        *self.llm.lock() = provider;
    }

    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        self.small_llm.lock().clone()
    }

    pub fn set_small_llm(&self, provider: Arc<dyn LlmProvider>) {
        *self.small_llm.lock() = provider;
    }

    pub fn model_id(&self) -> String {
        self.model_id.lock().clone()
    }

    pub fn set_model_id(&self, id: String) {
        *self.model_id.lock() = id;
    }

    pub fn file_observation_store(&self) -> Arc<dyn FileObservationStore> {
        Arc::clone(&self.file_observation_store)
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.tool_registry.lock().clone()
    }

    pub fn set_tool_registry(&self, registry: Arc<ToolRegistry>) {
        *self.tool_registry.lock() = registry;
    }

    pub fn background_tasks(&self) -> Arc<Mutex<BackgroundTaskManager>> {
        Arc::clone(&self.bg_tasks)
    }

    pub fn extra_system_prompt(&self) -> Option<String> {
        self.extra_system_prompt.lock().clone()
    }

    pub fn set_extra_system_prompt(&self, prompt: Option<String>) {
        *self.extra_system_prompt.lock() = prompt;
    }

    pub fn tool_policy(&self) -> Option<ChildToolPolicy> {
        self.tool_policy.lock().clone()
    }

    pub fn set_tool_policy(&self, policy: Option<ChildToolPolicy>) {
        *self.tool_policy.lock() = policy;
    }

    pub fn compact_circuit_breaker(&self) -> &Mutex<CompactCircuitBreaker> {
        &self.compact_circuit_breaker
    }

    pub fn configure_compact_circuit_breaker(&self, threshold: u32, cooldown: Duration) {
        self.compact_circuit_breaker
            .lock()
            .reconfigure(threshold, cooldown);
    }

    pub fn cached_stable_prefix(&self) -> Option<(String, String)> {
        self.cached_stable_prefix.lock().clone()
    }

    pub fn set_cached_stable_prefix(&self, text: String, fingerprint: String) {
        *self.cached_stable_prefix.lock() = Some((text, fingerprint));
    }

    /// 清空缓存的稳定前缀，强制下一 turn 全量重建（compact 后调用）。
    pub fn invalidate_stable_prefix_cache(&self) {
        *self.cached_stable_prefix.lock() = None;
    }

    /// 订阅本 session 的事件流。
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<Event> {
        self.event_out.subscribe()
    }

    /// 向本 session 的 fan-out 通道推一个事件。
    pub(crate) fn fanout(&self, event: Event) {
        self.event_out.send(event);
    }

    // ── 子 agent 管理 ──────────────────────────────────────────

    pub fn child_turn_manager(&self) -> &ChildTurnManager {
        &self.child_turn_manager
    }

    pub fn completed_tx(&self) -> mpsc::UnboundedSender<SessionId> {
        self.completed_tx.clone()
    }

    /// 消费完成信号通道并收集已完成的子 turn guard。非阻塞。
    pub fn drain_completed(&self) -> Vec<Arc<ChildTurnGuard>> {
        let mut rx = self.completed_rx.lock();
        while rx.try_recv().is_ok() {}
        self.child_turn_manager.collect_completed()
    }

    /// 主清理路径：同步 abort 所有子 turn 并等完成。
    /// 调用方需在持有 `SessionManager` 时级联递归孙子。
    pub fn abort_all_direct(&self) -> Vec<Arc<ChildTurnGuard>> {
        self.child_turn_manager.abort_all_direct()
    }
}

// ── 兜底清理 ───────────────────────────────────────────────────

impl Drop for SessionRuntimeState {
    fn drop(&mut self) {
        let guards = self.child_turn_manager.abort_all_direct();
        if !guards.is_empty() {
            tracing::warn!(
                count = guards.len(),
                "SessionRuntimeState dropped without prior cleanup; child turns force-aborted"
            );
        }
    }
}
