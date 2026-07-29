//! 跨 session 共享的运行时能力。
//!
//! `SessionRuntimeServices` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<SessionRuntimeServices>`，运行 turn 时按需读取。
//!
//! `llm` 与 `effective_config` 支持热替换：server 端配置变更时通过 `swap_llm` /
//! `update_effective` 原子更新，正在运行的 turn 在下一轮 LLM 调用前看到新值。
//! 快路径读取使用 `ArcSwap`，避免每个 turn 为获取 provider / config 快照进入读锁。

use std::sync::Arc;

use arc_swap::ArcSwap;
use astrcode_core::{
    config::EffectiveConfig,
    context::{ContextAssembler, PostCompactEnricher},
    llm::LlmProvider,
};
use astrcode_extension_sdk::runtime_ports::{
    PromptContributor, RuntimeSnapshotState, ToolCatalogProvider, TurnHooks,
};

use crate::SessionExtensionPorts;

pub struct SessionRuntimeServices {
    llm: ArcSwap<ProviderSlot>,
    /// 小模型 provider slot。未配置小模型时与主模型相同。
    small_llm: ArcSwap<ProviderSlot>,
    extension_ports: SessionExtensionPorts,
    context_assembler: Arc<dyn ContextAssembler>,
    post_compact_enricher: Arc<dyn PostCompactEnricher>,
    effective_config: ArcSwap<EffectiveConfig>,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
}

struct ProviderSlot {
    provider: Arc<dyn LlmProvider>,
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
            llm: ArcSwap::from_pointee(ProviderSlot { provider: llm }),
            small_llm: ArcSwap::from_pointee(ProviderSlot {
                provider: small_llm,
            }),
            extension_ports,
            context_assembler,
            post_compact_enricher,
            effective_config: ArcSwap::from_pointee(effective_config),
            tool_catalog,
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.llm.load_full().provider)
    }

    pub fn swap_llm(&self, new: Arc<dyn LlmProvider>) {
        self.llm.store(Arc::new(ProviderSlot { provider: new }));
    }

    /// 返回小模型 provider。
    ///
    /// 未配置小模型时返回的与主模型相同。
    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.small_llm.load_full().provider)
    }

    /// 热替换小模型 provider。
    pub fn swap_small_llm(&self, new: Arc<dyn LlmProvider>) {
        self.small_llm
            .store(Arc::new(ProviderSlot { provider: new }));
    }

    pub(crate) fn tool_catalog(&self) -> &dyn ToolCatalogProvider {
        self.tool_catalog.as_ref()
    }

    pub(crate) fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        self.extension_ports.runtime_snapshot_state()
    }

    pub(crate) fn prompt_contributor(&self) -> &dyn PromptContributor {
        self.extension_ports.prompt_contributor()
    }

    pub(crate) fn turn_hooks(&self) -> &dyn TurnHooks {
        self.extension_ports.turn_hooks()
    }

    pub fn turn_hooks_arc(&self) -> Arc<dyn TurnHooks> {
        self.extension_ports.turn_hooks_arc()
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

    pub fn update_effective(&self, new: EffectiveConfig) {
        self.effective_config.store(Arc::new(new));
    }

    /// 获取 session_ops 能力引用。
    pub fn session_ops(&self) -> Option<Arc<dyn astrcode_core::tool::SessionOperations>> {
        self.extension_ports.session_operations().session_ops()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{
            AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
            ProviderAuthScheme, ProviderWireFormat,
        },
        context::{
            CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn, CompactResult,
            CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput,
            PostCompactEnrichInput, PostCompactEnricher,
        },
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_extension_sdk::runtime_ports::NoopRuntimePorts;
    use tokio::sync::mpsc;

    use super::*;

    struct UnusedLlm;

    #[async_trait::async_trait]
    impl LlmProvider for UnusedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            unreachable!("runtime services test does not call llm")
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct CustomContextAssembler {
        settings: ContextSettings,
    }

    #[async_trait::async_trait]
    impl ContextAssembler for CustomContextAssembler {
        fn settings(&self) -> &ContextSettings {
            &self.settings
        }

        fn should_auto_compact(&self, _input: &ContextPrepareInput<'_>) -> bool {
            false
        }

        async fn compact_if_needed(
            &self,
            messages: Vec<LlmMessage>,
            _system_prompt: Option<&str>,
            _custom_instructions: &[String],
            _render_options: CompactSummaryRenderOptions,
            _options: CompactMessagesOptions,
            _request_text: CompactRequestFn,
        ) -> CompactIfNeededOutcome {
            CompactIfNeededOutcome::NotRun { messages }
        }
    }

    struct CountingPostCompactEnricher;

    #[async_trait::async_trait]
    impl PostCompactEnricher for CountingPostCompactEnricher {
        async fn enrich(&self, compaction: &mut CompactResult, _input: PostCompactEnrichInput<'_>) {
            compaction.summary.push_str(" enriched");
        }
    }

    #[tokio::test]
    async fn accepts_custom_context_services() {
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let context = ContextSettings {
            auto_compact_enabled: false,
            ..ContextSettings::default()
        };
        let context_assembler: Arc<dyn ContextAssembler> = Arc::new(CustomContextAssembler {
            settings: context.clone(),
        });

        let services = SessionRuntimeServices::new(
            llm.clone(),
            llm,
            effective_config(context),
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
            context_messages: Vec::new(),
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

    fn effective_config(context: ContextSettings) -> EffectiveConfig {
        let llm = LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
        };
        EffectiveConfig {
            llm: llm.clone(),
            small_llm: llm,
            context,
            agent: AgentSettings::default(),
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        }
    }
}
