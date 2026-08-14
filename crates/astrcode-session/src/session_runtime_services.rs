//! 跨 session 共享的运行时能力。
//!
//! `SessionRuntimeServices` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<SessionRuntimeServices>`，运行 turn 时按需读取。
//!
//! 配置相关能力按 generation 原子热替换。每个 turn 固定一代 effective config、provider 与
//! context assembler，避免配置重载时混用不同代的运行时对象。
//! 快路径读取使用 `ArcSwap`，避免每个 turn 为获取快照进入读锁。

use std::sync::Arc;

use arc_swap::ArcSwap;
use astrcode_context::{
    ContextAssembler, PostCompactEnricher, context_assembler::LlmContextAssembler,
};
use astrcode_core::{
    config::EffectiveConfig,
    llm::{
        LlmError, LlmEvent, LlmMessage, LlmProvider, LlmProviderBindings, LlmRequest, ModelLimits,
        ProviderInputTokenCount,
    },
    tool::ToolDefinition,
};
use astrcode_extension_sdk::runtime_ports::{RuntimeSnapshotState, TurnExtensionView};

use crate::{
    SessionExtensionPorts, SessionResourceStore,
    runtime_stability::{RuntimeStabilityBudget, retry_runtime_snapshot},
    session_error::SessionError,
};

pub struct SessionRuntimeServices {
    runtime_generation: Arc<ArcSwap<RuntimeGeneration>>,
    extension_ports: SessionExtensionPorts,
    post_compact_enricher: Arc<dyn PostCompactEnricher>,
    session_resources: SessionResourceStore,
}

struct RuntimeGeneration {
    llm: Arc<dyn LlmProvider>,
    small_llm: Arc<dyn LlmProvider>,
    effective_config: Arc<EffectiveConfig>,
    context_assembler: Arc<dyn ContextAssembler>,
    extension_generation: u64,
}

#[derive(Clone)]
pub(crate) struct RuntimeGenerationView {
    generation: Arc<RuntimeGeneration>,
}

impl RuntimeGenerationView {
    pub(crate) fn effective(&self) -> &EffectiveConfig {
        &self.generation.effective_config
    }

    pub(crate) fn llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.generation.llm)
    }

    pub(crate) fn llm_for_model_id(&self, model_id: &str) -> Arc<dyn LlmProvider> {
        let effective = self.effective();
        if model_id == effective.small_llm.model_id && model_id != effective.llm.model_id {
            Arc::clone(&self.generation.small_llm)
        } else {
            self.llm()
        }
    }

    pub(crate) fn llm_bindings_for_model_id(&self, model_id: &str) -> LlmProviderBindings {
        LlmProviderBindings::new(
            self.llm_for_model_id(model_id),
            Arc::clone(&self.generation.small_llm),
        )
    }

    pub(crate) fn context_assembler(&self) -> &Arc<dyn ContextAssembler> {
        &self.generation.context_assembler
    }

    pub(crate) fn max_parallel_tool_calls(&self) -> usize {
        self.effective().agent.tool_max_parallel_calls.max(1)
    }
}

#[derive(Clone, Copy)]
enum ProviderKind {
    Main,
    Small,
}

struct LiveLlmProvider {
    source: Arc<ArcSwap<RuntimeGeneration>>,
    kind: ProviderKind,
}

impl LiveLlmProvider {
    fn current(&self) -> Arc<dyn LlmProvider> {
        let generation = self.source.load_full();
        match self.kind {
            ProviderKind::Main => Arc::clone(&generation.llm),
            ProviderKind::Small => Arc::clone(&generation.small_llm),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for LiveLlmProvider {
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
    /// Derives the context assembler from `effective_config.context`, matching
    /// what [`Self::publish_runtime_generation`] installs on config updates.
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        effective_config: EffectiveConfig,
        extension_ports: SessionExtensionPorts,
        post_compact_enricher: Arc<dyn PostCompactEnricher>,
    ) -> Self {
        let context_assembler =
            Arc::new(LlmContextAssembler::new(effective_config.context.clone()));
        Self::new_with_context_assembler(
            llm,
            small_llm,
            effective_config,
            extension_ports,
            context_assembler,
            post_compact_enricher,
        )
    }

    /// Installs a caller-provided context assembler for the initial runtime
    /// generation. Intended for test doubles: the next config publication
    /// rebuilds the assembler from the new effective config.
    pub fn new_with_context_assembler(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        effective_config: EffectiveConfig,
        extension_ports: SessionExtensionPorts,
        context_assembler: Arc<dyn ContextAssembler>,
        post_compact_enricher: Arc<dyn PostCompactEnricher>,
    ) -> Self {
        let extension_generation = match extension_ports.runtime_snapshot_state() {
            RuntimeSnapshotState::Stable(generation) => generation,
            RuntimeSnapshotState::Updating => 0,
        };
        Self {
            runtime_generation: Arc::new(ArcSwap::from_pointee(RuntimeGeneration {
                llm,
                small_llm,
                effective_config: Arc::new(effective_config),
                context_assembler,
                extension_generation,
            })),
            extension_ports,
            post_compact_enricher,
            session_resources: SessionResourceStore::default(),
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.pin_runtime_generation().llm()
    }

    /// 返回始终转发到当前主模型 provider 的稳定句柄。
    pub fn live_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::new(LiveLlmProvider {
            source: Arc::clone(&self.runtime_generation),
            kind: ProviderKind::Main,
        })
    }

    /// 返回小模型 provider。
    ///
    /// 返回的是当前 generation 中配置的实例；"未配置小模型时按主模型处理"由 turn 固定
    /// generation 后按模型 id 判定，不在本方法内。
    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.runtime_generation.load_full().small_llm)
    }

    /// 返回始终转发到当前小模型 provider 的稳定句柄。
    pub fn live_small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::new(LiveLlmProvider {
            source: Arc::clone(&self.runtime_generation),
            kind: ProviderKind::Small,
        })
    }

    pub(crate) async fn pin_extension_view(&self) -> Result<TurnExtensionView, SessionError> {
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
            return Ok(extension);
        }
    }

    pub(crate) async fn pin_turn_generation(
        &self,
    ) -> Result<(RuntimeGenerationView, TurnExtensionView), SessionError> {
        let mut stability = RuntimeStabilityBudget::new();
        loop {
            let generation = self.runtime_generation.load_full();
            let RuntimeSnapshotState::Stable(extension_generation) =
                self.extension_ports.runtime_snapshot_state()
            else {
                retry_runtime_snapshot(&mut stability).await?;
                continue;
            };
            if generation.extension_generation != extension_generation {
                retry_runtime_snapshot(&mut stability).await?;
                continue;
            }
            let extension = self.extension_ports.turn_extension_view();
            let current_generation = self.runtime_generation.load_full();
            if extension.generation() != extension_generation
                || self.extension_ports.runtime_snapshot_state()
                    != RuntimeSnapshotState::Stable(extension_generation)
                || !Arc::ptr_eq(&generation, &current_generation)
            {
                retry_runtime_snapshot(&mut stability).await?;
                continue;
            }
            return Ok((RuntimeGenerationView { generation }, extension));
        }
    }

    pub(crate) fn post_compact_enricher(&self) -> &dyn PostCompactEnricher {
        self.post_compact_enricher.as_ref()
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        Arc::clone(&self.runtime_generation.load_full().effective_config)
    }

    pub(crate) fn pin_runtime_generation(&self) -> RuntimeGenerationView {
        RuntimeGenerationView {
            generation: self.runtime_generation.load_full(),
        }
    }

    pub fn publish_runtime_generation(
        &self,
        effective_config: EffectiveConfig,
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
    ) {
        let extension_generation = self.runtime_generation.load_full().extension_generation;
        self.publish_runtime_generation_for_extension(
            effective_config,
            llm,
            small_llm,
            extension_generation,
        );
    }

    pub fn publish_runtime_generation_for_extension(
        &self,
        effective_config: EffectiveConfig,
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        extension_generation: u64,
    ) {
        let context_assembler =
            Arc::new(LlmContextAssembler::new(effective_config.context.clone()));
        self.runtime_generation.store(Arc::new(RuntimeGeneration {
            llm,
            small_llm,
            effective_config: Arc::new(effective_config),
            context_assembler,
            extension_generation,
        }));
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

    use astrcode_context::{CompactResult, ContextAssembler, PostCompactEnrichInput};
    use astrcode_core::{
        config::ContextSettings,
        llm::{LlmError, LlmEvent, LlmProvider, LlmRequest, ModelLimits},
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

    struct CoordinatedRuntime {
        updating: AtomicBool,
        generation: AtomicU64,
    }

    impl RuntimeSnapshotProvider for CoordinatedRuntime {
        fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
            if self.updating.load(Ordering::Acquire) {
                RuntimeSnapshotState::Updating
            } else {
                RuntimeSnapshotState::Stable(self.generation.load(Ordering::Acquire))
            }
        }
    }

    impl TurnExtensionViewProvider for CoordinatedRuntime {
        fn turn_extension_view(&self) -> TurnExtensionView {
            let noop = Arc::new(NoopRuntimePorts);
            TurnExtensionView::new(
                self.generation.load(Ordering::Acquire),
                noop.clone(),
                noop.clone(),
                noop,
            )
        }
    }

    impl SessionOperationsProvider for CoordinatedRuntime {}

    #[async_trait::async_trait]
    impl LlmProvider for TaggedLlm {
        async fn generate_request(
            &self,
            _request: LlmRequest,
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

        let services = SessionRuntimeServices::new_with_context_assembler(
            llm.clone(),
            llm,
            test_effective_config(context),
            SessionExtensionPorts::default(),
            Arc::clone(&context_assembler),
            Arc::new(CountingPostCompactEnricher),
        );

        let runtime_generation = services.pin_runtime_generation();
        let active_context = runtime_generation.context_assembler();
        assert!(!active_context.auto_compact_enabled());
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
                    settings: active_context.settings(),
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
        let services = SessionRuntimeServices::new_with_context_assembler(
            llm.clone(),
            llm,
            test_effective_config(context.clone()),
            SessionExtensionPorts::from_adapter(Arc::clone(&extension_runtime)),
            Arc::new(NoopContextAssembler::new(context)),
            Arc::new(CountingPostCompactEnricher),
        );
        let runtime_for_update = Arc::clone(&extension_runtime);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            runtime_for_update.updating.store(false, Ordering::Release);
        });

        let view = services.pin_extension_view().await.unwrap();

        assert_eq!(view.generation(), 2);
        assert_eq!(extension_runtime.view_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn turn_pin_never_mixes_core_and_extension_publication_epochs() {
        let extension_runtime = Arc::new(CoordinatedRuntime {
            updating: AtomicBool::new(false),
            generation: AtomicU64::new(1),
        });
        let context = ContextSettings::default();
        let mut old_effective = test_effective_config(context.clone());
        old_effective.llm.model_id = "old-main".into();
        let services = Arc::new(SessionRuntimeServices::new_with_context_assembler(
            Arc::new(TaggedLlm {
                max_input_tokens: 1,
            }),
            Arc::new(TaggedLlm {
                max_input_tokens: 2,
            }),
            old_effective,
            SessionExtensionPorts::from_adapter(Arc::clone(&extension_runtime)),
            Arc::new(NoopContextAssembler::new(context.clone())),
            Arc::new(CountingPostCompactEnricher),
        ));

        extension_runtime.updating.store(true, Ordering::Release);
        let publisher_services = Arc::clone(&services);
        let publisher_runtime = Arc::clone(&extension_runtime);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let mut effective = test_effective_config(context);
            effective.llm.model_id = "new-main".into();
            publisher_services.publish_runtime_generation_for_extension(
                effective,
                Arc::new(TaggedLlm {
                    max_input_tokens: 3,
                }),
                Arc::new(TaggedLlm {
                    max_input_tokens: 4,
                }),
                2,
            );
            publisher_runtime.generation.store(2, Ordering::Release);
            publisher_runtime.updating.store(false, Ordering::Release);
        });

        let (core, extension) = services.pin_turn_generation().await.unwrap();

        assert_eq!(core.effective().llm.model_id, "new-main");
        assert_eq!(core.llm().model_limits().max_input_tokens, 3);
        assert_eq!(extension.generation(), 2);
    }

    #[test]
    fn runtime_generation_is_pinned_per_turn_and_live_handles_follow_publication() {
        let context = ContextSettings {
            compact_threshold_percent: 40.0,
            ..ContextSettings::default()
        };
        let mut effective = test_effective_config(context.clone());
        effective.llm.model_id = "old-main".into();
        effective.small_llm.model_id = "old-small".into();
        let services = SessionRuntimeServices::new(
            Arc::new(TaggedLlm {
                max_input_tokens: 1,
            }),
            Arc::new(TaggedLlm {
                max_input_tokens: 2,
            }),
            effective,
            SessionExtensionPorts::default(),
            Arc::new(CountingPostCompactEnricher),
        );
        let live_main = services.live_llm();
        let live_small = services.live_small_llm();
        let old_generation = services.pin_runtime_generation();

        assert_eq!(live_main.model_limits().max_input_tokens, 1);
        assert_eq!(live_small.model_limits().max_input_tokens, 2);
        assert_eq!(old_generation.effective().llm.model_id, "old-main");
        assert_eq!(
            old_generation
                .context_assembler()
                .settings()
                .compact_threshold_percent,
            40.0
        );

        let new_context = ContextSettings {
            compact_threshold_percent: 80.0,
            ..ContextSettings::default()
        };
        let mut new_effective = test_effective_config(new_context.clone());
        new_effective.llm.model_id = "new-main".into();
        new_effective.small_llm.model_id = "new-small".into();
        services.publish_runtime_generation(
            new_effective,
            Arc::new(TaggedLlm {
                max_input_tokens: 3,
            }),
            Arc::new(TaggedLlm {
                max_input_tokens: 4,
            }),
        );
        let new_generation = services.pin_runtime_generation();

        assert_eq!(live_main.model_limits().max_input_tokens, 3);
        assert_eq!(live_small.model_limits().max_input_tokens, 4);
        assert_eq!(old_generation.effective().llm.model_id, "old-main");
        assert_eq!(
            old_generation
                .llm_for_model_id("old-small")
                .model_limits()
                .max_input_tokens,
            2
        );
        assert_eq!(
            old_generation
                .context_assembler()
                .settings()
                .compact_threshold_percent,
            40.0
        );
        assert_eq!(new_generation.effective().llm.model_id, "new-main");
        assert_eq!(
            new_generation
                .llm_for_model_id("new-small")
                .model_limits()
                .max_input_tokens,
            4
        );
        assert_eq!(
            new_generation
                .context_assembler()
                .settings()
                .compact_threshold_percent,
            80.0
        );
    }
}
