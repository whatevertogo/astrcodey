//! 跨 session 共享的运行时能力。
//!
//! `SessionRuntimeServices` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<SessionRuntimeServices>`，运行 turn 时按需读取。
//!
//! `llm` 与 `effective_config` 支持热替换：server 端配置变更时通过 `swap_llm` /
//! `update_effective` 原子更新，后续 turn 会读取新值。
//! 快路径读取使用 `ArcSwap`，避免每个 turn 为获取 provider / config 快照进入读锁。

use std::sync::Arc;

use arc_swap::ArcSwap;
use astrcode_context::{ContextAssembler, PostCompactEnricher};
use astrcode_core::{
    config::EffectiveConfig,
    llm::{
        LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRequest, ModelLimits,
        ProviderInputTokenCount,
    },
    tool::ToolDefinition,
};
use astrcode_extension_sdk::runtime_ports::{
    CompositeToolCatalogProvider, PromptContributor, RuntimeSnapshotState, ToolCatalogProvider,
    TurnExtensionView, TurnHooks,
};

use crate::{
    SessionExtensionPorts, SessionResourceStore,
    runtime_stability::{RuntimeStabilityBudget, retry_runtime_snapshot},
    session_error::SessionError,
};

pub struct SessionRuntimeServices {
    llm: Arc<ArcSwap<ProviderSlot>>,
    /// 小模型 provider slot。
    ///
    /// slot 本身不实现"未配置时回退主模型"：该回退发生在调用方
    /// [`Self::llm_for_model_id`]（按生效配置判定：小模型 id 与主模型 id 相同即
    /// 视为未配置，走主模型）。这里存放的是 server 构建时传入的 provider 实例。
    small_llm: Arc<ArcSwap<ProviderSlot>>,
    extension_ports: SessionExtensionPorts,
    context_assembler: Arc<dyn ContextAssembler>,
    post_compact_enricher: Arc<dyn PostCompactEnricher>,
    effective_config: ArcSwap<EffectiveConfig>,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
    session_resources: SessionResourceStore,
}

/// Extension ports and the combined tool catalog pinned to one generation.
pub(crate) struct SessionRuntimeView {
    extension: TurnExtensionView,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
}

impl SessionRuntimeView {
    pub(crate) fn tool_catalog(&self) -> &dyn ToolCatalogProvider {
        self.tool_catalog.as_ref()
    }

    pub(crate) fn prompt_contributor(&self) -> &dyn PromptContributor {
        self.extension.prompt_contributor()
    }

    pub(crate) fn turn_hooks(&self) -> &dyn TurnHooks {
        self.extension.turn_hooks()
    }

    pub(crate) fn turn_hooks_arc(&self) -> Arc<dyn TurnHooks> {
        self.extension.turn_hooks_arc()
    }
}

struct ProviderSlot {
    provider: Arc<dyn LlmProvider>,
}

/// 主/小模型各持有一份完全相同的 `LiveLlmProvider` 实现，仅绑定的 slot 不同。
/// 修改本类型时两处（[`SessionRuntimeServices::live_llm`] 与
/// [`SessionRuntimeServices::live_small_llm`] 返回的实例）必须同步；若后续差异
/// 增多，应收敛为按 slot 泛型的单一实现。
struct LiveLlmProvider {
    source: Arc<ArcSwap<ProviderSlot>>,
}

impl LiveLlmProvider {
    fn current(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.source.load_full().provider)
    }
}

#[async_trait::async_trait]
impl LlmProvider for LiveLlmProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        self.current().generate(messages, tools).await
    }

    async fn generate_request(
        &self,
        request: LlmRequest,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        self.current().generate_request(request).await
    }

    async fn count_input_tokens(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        self.current().count_input_tokens(messages, tools).await
    }

    fn minimum_output_tokens(&self) -> usize {
        self.current().minimum_output_tokens()
    }

    fn model_limits(&self) -> ModelLimits {
        self.current().model_limits()
    }
}

impl SessionRuntimeServices {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        effective_config: EffectiveConfig,
        extension_ports: SessionExtensionPorts,
        context_assembler: Arc<dyn ContextAssembler>,
        post_compact_enricher: Arc<dyn PostCompactEnricher>,
        tool_catalog: Arc<dyn ToolCatalogProvider>,
    ) -> Self {
        Self {
            llm: Arc::new(ArcSwap::from_pointee(ProviderSlot { provider: llm })),
            small_llm: Arc::new(ArcSwap::from_pointee(ProviderSlot {
                provider: small_llm,
            })),
            extension_ports,
            context_assembler,
            post_compact_enricher,
            effective_config: ArcSwap::from_pointee(effective_config),
            tool_catalog,
            session_resources: SessionResourceStore::default(),
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.llm.load_full().provider)
    }

    pub fn swap_llm(&self, new: Arc<dyn LlmProvider>) {
        self.llm.store(Arc::new(ProviderSlot { provider: new }));
    }

    /// 返回始终转发到当前主模型 provider 的稳定句柄。
    pub fn live_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::new(LiveLlmProvider {
            source: Arc::clone(&self.llm),
        })
    }

    /// 返回小模型 provider。
    ///
    /// 返回的是 slot 中配置的实例；"未配置小模型时按主模型处理"的回退在
    /// [`Self::llm_for_model_id`] 中按模型 id 判定，不在本方法内。
    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.small_llm.load_full().provider)
    }

    pub fn llm_for_model_id(&self, model_id: &str) -> Arc<dyn LlmProvider> {
        let effective = self.read_effective();
        if model_id == effective.small_llm.model_id && model_id != effective.llm.model_id {
            self.small_llm()
        } else {
            self.llm()
        }
    }

    /// 热替换小模型 provider。
    pub fn swap_small_llm(&self, new: Arc<dyn LlmProvider>) {
        self.small_llm
            .store(Arc::new(ProviderSlot { provider: new }));
    }

    /// 返回始终转发到当前小模型 provider 的稳定句柄。
    pub fn live_small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::new(LiveLlmProvider {
            source: Arc::clone(&self.small_llm),
        })
    }

    /// 直接读取当前 turn hooks，不等待 runtime 稳定窗口。
    ///
    /// 只用于生命周期事件等不需要固定 tool catalog / generation 的路径。
    pub(crate) fn turn_hooks(&self) -> Arc<dyn TurnHooks> {
        self.extension_ports.turn_extension_view().turn_hooks_arc()
    }

    pub(crate) async fn turn_runtime_view(&self) -> Result<SessionRuntimeView, SessionError> {
        let mut stability = RuntimeStabilityBudget::new();
        loop {
            let RuntimeSnapshotState::Stable(generation) =
                self.extension_ports.runtime_snapshot_state()
            else {
                retry_runtime_snapshot(&mut stability).await?;
                continue;
            };
            let extension = self.extension_ports.turn_extension_view();
            if extension.generation() != generation
                || self.extension_ports.runtime_snapshot_state()
                    != RuntimeSnapshotState::Stable(generation)
            {
                retry_runtime_snapshot(&mut stability).await?;
                continue;
            }
            let extension_catalog = extension.tool_catalog_arc();
            let tool_catalog: Arc<dyn ToolCatalogProvider> =
                Arc::new(CompositeToolCatalogProvider::new(vec![
                    ("extensions".into(), extension_catalog),
                    ("builtins".into(), Arc::clone(&self.tool_catalog)),
                ]));
            return Ok(SessionRuntimeView {
                extension,
                tool_catalog,
            });
        }
    }

    pub(crate) fn context_assembler(&self) -> &dyn ContextAssembler {
        self.context_assembler.as_ref()
    }

    pub fn context_assembler_arc(&self) -> Arc<dyn ContextAssembler> {
        Arc::clone(&self.context_assembler)
    }

    pub(crate) fn post_compact_enricher(&self) -> &dyn PostCompactEnricher {
        self.post_compact_enricher.as_ref()
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        self.effective_config.load_full()
    }

    /// 单轮允许的并行工具调用上限（至少 1）。
    ///
    /// turn 调度与工具批量执行共用同一口径，避免两处各自读取配置。
    pub fn max_parallel_tool_calls(&self) -> usize {
        self.read_effective().agent.tool_max_parallel_calls.max(1)
    }

    pub fn update_effective(&self, new: EffectiveConfig) {
        self.effective_config.store(Arc::new(new));
    }

    /// 获取 session_ops 能力引用。
    pub fn session_ops(&self) -> Option<Arc<dyn astrcode_core::tool::SessionOperations>> {
        self.extension_ports.session_operations().session_ops()
    }

    pub fn session_resources(&self) -> &SessionResourceStore {
        &self.session_resources
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        time::Duration,
    };

    use astrcode_context::{
        CompactResult, ContextAssembler, PostCompactEnrichInput,
        context_assembler::LlmContextAssembler,
    };
    use astrcode_core::{
        config::ContextSettings,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_extension_sdk::runtime_ports::{
        NoopRuntimePorts, RuntimeSnapshotProvider, SessionOperationsProvider, TurnExtensionView,
        TurnExtensionViewProvider,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::test_support::{
        CountingPostCompactEnricher, NoopContextAssembler, UnusedLlm, test_effective_config,
    };

    struct TaggedLlm {
        max_input_tokens: usize,
    }

    struct StabilizingRuntime {
        updating: AtomicBool,
        generation: AtomicU64,
        view_calls: AtomicUsize,
    }

    impl RuntimeSnapshotProvider for StabilizingRuntime {
        fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
            if self.updating.load(Ordering::Acquire) {
                RuntimeSnapshotState::Updating
            } else {
                RuntimeSnapshotState::Stable(self.generation.load(Ordering::Acquire))
            }
        }
    }

    impl TurnExtensionViewProvider for StabilizingRuntime {
        fn turn_extension_view(&self) -> TurnExtensionView {
            let generation = self.generation.load(Ordering::Acquire);
            if self.view_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                self.generation.store(generation + 1, Ordering::Release);
            }
            let noop = Arc::new(NoopRuntimePorts);
            TurnExtensionView::new(generation, noop.clone(), noop.clone(), noop)
        }
    }

    impl SessionOperationsProvider for StabilizingRuntime {}

    #[async_trait::async_trait]
    impl LlmProvider for TaggedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            unreachable!("live provider test only reads model limits")
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: self.max_input_tokens,
                max_output_tokens: 1024,
            }
        }
    }

    #[tokio::test]
    async fn accepts_custom_context_services() {
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let context = ContextSettings {
            auto_compact_enabled: false,
            ..ContextSettings::default()
        };
        let context_assembler: Arc<dyn ContextAssembler> =
            Arc::new(NoopContextAssembler::new(context.clone()));

        let services = SessionRuntimeServices::new(
            llm.clone(),
            llm,
            test_effective_config(context),
            SessionExtensionPorts::default(),
            Arc::clone(&context_assembler),
            Arc::new(CountingPostCompactEnricher),
            Arc::new(NoopRuntimePorts),
        );

        assert!(!services.context_assembler().auto_compact_enabled());
        let mut compaction = CompactResult {
            pre_tokens: 1,
            post_tokens: 1,
            summary: "compact".into(),
            messages_removed: 0,
            summary_messages: Vec::new(),
            retained_messages: Vec::new(),
            transcript_path: None,
        };
        services
            .post_compact_enricher()
            .enrich(
                &mut compaction,
                PostCompactEnrichInput {
                    session_id: "session-test",
                    source_messages: &[],
                    working_dir: ".",
                    system_prompt: None,
                    tools: &[],
                    settings: services.context_assembler().settings(),
                    session_store_dir: None,
                },
            )
            .await;
        assert_eq!(compaction.summary, "compact enriched");
    }

    #[tokio::test]
    async fn turn_view_waits_for_stability_and_rechecks_generation() {
        let extension_runtime = Arc::new(StabilizingRuntime {
            updating: AtomicBool::new(true),
            generation: AtomicU64::new(1),
            view_calls: AtomicUsize::new(0),
        });
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let context = ContextSettings::default();
        let services = SessionRuntimeServices::new(
            llm.clone(),
            llm,
            test_effective_config(context.clone()),
            SessionExtensionPorts::from_adapter(Arc::clone(&extension_runtime)),
            Arc::new(NoopContextAssembler::new(context)),
            Arc::new(CountingPostCompactEnricher),
            Arc::new(NoopRuntimePorts),
        );
        let runtime_for_update = Arc::clone(&extension_runtime);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            runtime_for_update.updating.store(false, Ordering::Release);
        });

        let view = services.turn_runtime_view().await.unwrap();

        assert_eq!(view.extension.generation(), 2);
        assert_eq!(extension_runtime.view_calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn live_llm_handles_follow_main_and_small_provider_swaps() {
        let context = ContextSettings::default();
        let context_assembler: Arc<dyn ContextAssembler> =
            Arc::new(LlmContextAssembler::new(context.clone()));
        let services = SessionRuntimeServices::new(
            Arc::new(TaggedLlm {
                max_input_tokens: 1,
            }),
            Arc::new(TaggedLlm {
                max_input_tokens: 2,
            }),
            test_effective_config(context),
            SessionExtensionPorts::default(),
            context_assembler,
            Arc::new(CountingPostCompactEnricher),
            Arc::new(NoopRuntimePorts),
        );
        let live_main = services.live_llm();
        let live_small = services.live_small_llm();

        assert_eq!(live_main.model_limits().max_input_tokens, 1);
        assert_eq!(live_small.model_limits().max_input_tokens, 2);

        services.swap_llm(Arc::new(TaggedLlm {
            max_input_tokens: 3,
        }));
        services.swap_small_llm(Arc::new(TaggedLlm {
            max_input_tokens: 4,
        }));

        assert_eq!(live_main.model_limits().max_input_tokens, 3);
        assert_eq!(live_small.model_limits().max_input_tokens, 4);
    }
}
