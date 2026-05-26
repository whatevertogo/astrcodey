use std::{sync::Arc, time::Duration};

use astrcode_core::{
    event::Event, extension::ChildToolPolicy, llm::LlmProvider, tool::FileObservationStore,
    types::SessionId,
};
use astrcode_support::{event_fanout::EventFanout, sync::lock_parking};
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    background::BackgroundTaskManager,
    child_turn::{ChildTurnGuard, ChildTurnManager},
    compact_circuit_breaker::CompactCircuitBreaker,
    tool_exec::InMemoryFileObservationStore,
};

/// 当前 session 使用的模型绑定；一次替换同时切换 provider 与模型标识。
#[derive(Clone)]
pub struct SessionModelBinding {
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) small_llm: Arc<dyn LlmProvider>,
    pub(crate) model_id: String,
}

impl SessionModelBinding {
    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
    }

    pub fn small_llm(&self) -> &Arc<dyn LlmProvider> {
        &self.small_llm
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// 执行工具所需的进程内资源。
struct ToolResources {
    file_observation_store: Arc<dyn FileObservationStore>,
    registry: Mutex<Arc<ToolRegistry>>,
    background_tasks: Arc<Mutex<BackgroundTaskManager>>,
}

/// 参与每次 turn 装配的可变配置与其派生缓存。
struct TurnConfiguration {
    /// 子 session 专用的额外 system prompt，由 SpawnRequest 注入。
    extra_system_prompt: Mutex<Option<String>>,
    /// 子 session 工具集策略，由 SpawnRequest 注入；父 session 始终为 `None`。
    ///
    /// `refresh_tools` 在每次重建工具表时读取此字段，保证子 session 的所有 turn
    /// 都看到一致的裁剪后工具集（含 resume 路径）。
    tool_policy: Mutex<Option<ChildToolPolicy>>,
    /// 缓存的稳定前缀文本及其指纹（Identity → ProjectRules）。
    /// 首次构建后跨 turn 复用，compact 后清空触发全量重建。
    cached_stable_prefix: Mutex<Option<(String, String)>>,
}

/// 派生子 agent turn 的所有权与完成通知通道。
struct ChildTurns {
    manager: ChildTurnManager,
    completed_tx: mpsc::UnboundedSender<SessionId>,
    completed_rx: Mutex<mpsc::UnboundedReceiver<SessionId>>,
}

impl ChildTurns {
    fn new() -> Self {
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        Self {
            manager: ChildTurnManager::new(),
            completed_tx,
            completed_rx: Mutex::new(completed_rx),
        }
    }
}

/// 单个 session 在当前进程内持有的瞬态状态。
///
/// 这里的状态随 session 生命周期存在，但不属于可持久化事实；此类型仅组合按职责
/// 拆分的运行态组件，并作为 session 调用方的稳定门面。
///
/// `event_out` 故意放在 `SessionRuntimeState` 而非 `Session`：同一 sid 多次
/// `Session::open` 会得到多个 `Session` 实例（廉价的 store handle clone），
/// 但所有实例必须共享同一份 `SessionRuntimeState`（含 EventFanout）才能让所有订阅者
/// 看到全部事件。SessionRuntimeRegistry / SessionManager 保证 per-sid 唯一。
///
/// 注意：直接通过 `Session::create_with_id` 而绕过 `SessionRuntimeRegistry` 创建的 session
/// 会得到独立 runtime，订阅者不会跨实例可见——这是给 `spawn_child` 这类「我就是要新 runtime」
/// 的场景用的。SessionManager 走 registry 路径。
pub struct SessionRuntimeState {
    model: Mutex<SessionModelBinding>,
    tools: ToolResources,
    configuration: TurnConfiguration,
    /// 熔断器需要 &mut self 的状态转换（Open→HalfOpen）。
    compact_circuit_breaker: Mutex<CompactCircuitBreaker>,
    /// 本 session 事件的 fan-out 通道。同一 sid 下所有 Session 实例共享这份 sender，
    /// 通过 SessionRuntimeState 的 Arc 共享保证订阅一致性。
    event_out: Arc<EventFanout<Event>>,
    children: ChildTurns,
}

impl SessionRuntimeState {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        model_id: String,
    ) -> Self {
        let event_out = Arc::new(EventFanout::new(1024));
        Self {
            model: Mutex::new(SessionModelBinding {
                llm,
                small_llm,
                model_id,
            }),
            tools: ToolResources {
                file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
                registry: Mutex::new(Arc::new(ToolRegistry::new())),
                background_tasks: Arc::new(Mutex::new(BackgroundTaskManager::new())),
            },
            configuration: TurnConfiguration {
                extra_system_prompt: Mutex::new(None),
                tool_policy: Mutex::new(None),
                cached_stable_prefix: Mutex::new(None),
            },
            compact_circuit_breaker: Mutex::new(CompactCircuitBreaker::new(
                3,
                Duration::from_secs(60),
            )),
            event_out,
            children: ChildTurns::new(),
        }
    }

    /// 返回 provider 与模型标识的一致快照。
    ///
    /// 需要同时读取 `llm` / `small_llm` / `model_id` 时请用此方法；
    /// 分别调用 [`Self::llm`]、[`Self::small_llm`] 可能在替换间隙读到不一致组合。
    pub fn model_binding(&self) -> SessionModelBinding {
        lock_parking(&self.model).clone()
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&lock_parking(&self.model).llm)
    }

    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&lock_parking(&self.model).small_llm)
    }

    pub fn model_id(&self) -> String {
        lock_parking(&self.model).model_id.clone()
    }

    pub fn replace_model_binding(
        &self,
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        model_id: String,
    ) {
        *lock_parking(&self.model) = SessionModelBinding {
            llm,
            small_llm,
            model_id,
        };
    }

    pub fn file_observation_store(&self) -> Arc<dyn FileObservationStore> {
        Arc::clone(&self.tools.file_observation_store)
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(&lock_parking(&self.tools.registry))
    }

    pub fn set_tool_registry(&self, registry: Arc<ToolRegistry>) {
        *lock_parking(&self.tools.registry) = registry;
    }

    pub fn background_tasks(&self) -> Arc<Mutex<BackgroundTaskManager>> {
        Arc::clone(&self.tools.background_tasks)
    }

    pub fn extra_system_prompt(&self) -> Option<String> {
        lock_parking(&self.configuration.extra_system_prompt).clone()
    }

    pub fn set_extra_system_prompt(&self, prompt: Option<String>) {
        *lock_parking(&self.configuration.extra_system_prompt) = prompt;
    }

    pub fn tool_policy(&self) -> Option<ChildToolPolicy> {
        lock_parking(&self.configuration.tool_policy).clone()
    }

    pub fn set_tool_policy(&self, policy: Option<ChildToolPolicy>) {
        *lock_parking(&self.configuration.tool_policy) = policy;
    }

    pub fn compact_circuit_breaker(&self) -> &Mutex<CompactCircuitBreaker> {
        &self.compact_circuit_breaker
    }

    pub fn configure_compact_circuit_breaker(&self, threshold: u32, cooldown: Duration) {
        lock_parking(&self.compact_circuit_breaker).reconfigure(threshold, cooldown);
    }

    pub fn cached_stable_prefix(&self) -> Option<(String, String)> {
        lock_parking(&self.configuration.cached_stable_prefix).clone()
    }

    pub fn set_cached_stable_prefix(&self, text: String, fingerprint: String) {
        *lock_parking(&self.configuration.cached_stable_prefix) = Some((text, fingerprint));
    }

    /// 清空缓存的稳定前缀，强制下一 turn 全量重建（compact 后调用）。
    pub fn invalidate_stable_prefix_cache(&self) {
        *lock_parking(&self.configuration.cached_stable_prefix) = None;
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
        &self.children.manager
    }

    pub fn completed_tx(&self) -> mpsc::UnboundedSender<SessionId> {
        self.children.completed_tx.clone()
    }

    /// 消费完成信号通道并收集已完成的子 turn guard。非阻塞。
    /// 消费子 turn 完成通知 channel 中积压的 signal，再收集已完成的 guard。
    /// signal 本身不含 payload，仅作唤醒；真正状态在 `ChildTurnManager` 内。
    pub fn drain_completed(&self) -> Vec<Arc<ChildTurnGuard>> {
        let mut rx = self.children.completed_rx.lock();
        while rx.try_recv().is_ok() {}
        self.children.manager.collect_completed()
    }

    /// 主清理路径：同步 abort 所有子 turn 并等完成。
    /// 调用方需在持有 `SessionManager` 时级联递归孙子。
    pub fn abort_all_direct(&self) -> Vec<Arc<ChildTurnGuard>> {
        self.children.manager.abort_all_direct()
    }
}

// ── 兜底清理 ───────────────────────────────────────────────────

impl Drop for SessionRuntimeState {
    fn drop(&mut self) {
        let guards = self.children.manager.abort_all_direct();
        if !guards.is_empty() {
            tracing::warn!(
                count = guards.len(),
                "SessionRuntimeState dropped without prior cleanup; child turns force-aborted"
            );
        }
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

    fn assert_consistent_binding(binding: &SessionModelBinding) {
        let tag: usize = binding.model_id().parse().unwrap();
        assert_eq!(binding.llm().model_limits().max_input_tokens, tag);
        assert_eq!(
            binding.small_llm().model_limits().max_input_tokens,
            tag + 1000
        );
    }

    #[test]
    fn model_binding_replacement_is_atomic() {
        let runtime = Arc::new(SessionRuntimeState::new(
            provider(1),
            provider(1001),
            "1".to_string(),
        ));
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
}
