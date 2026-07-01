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
    context::{ContextAssembler, NoopPostCompactEnricher, PostCompactEnricher},
    llm::LlmProvider,
    prompt::{PromptFileProvider, PromptProvider},
};
use astrcode_kernel::{ExtensionRuntime, ToolPack, extension_runtime::NoopExtensionRuntime};

pub struct SessionRuntimeServices {
    llm: ArcSwap<ProviderSlot>,
    /// 小模型 provider slot。未配置小模型时与主模型相同。
    small_llm: ArcSwap<ProviderSlot>,
    extension_runner: Arc<dyn ExtensionRuntime>,
    context_assembler: Arc<dyn ContextAssembler>,
    post_compact_enricher: Arc<dyn PostCompactEnricher>,
    prompt_provider: Arc<dyn PromptProvider>,
    prompt_file_provider: Arc<dyn PromptFileProvider>,
    effective_config: ArcSwap<EffectiveConfig>,
    tool_packs: Arc<[Arc<dyn ToolPack>]>,
}

pub struct SessionHostServices {
    pub extension_runner: Arc<dyn ExtensionRuntime>,
    pub context_assembler: Arc<dyn ContextAssembler>,
    pub post_compact_enricher: Arc<dyn PostCompactEnricher>,
    pub prompt_provider: Arc<dyn PromptProvider>,
    pub prompt_file_provider: Arc<dyn PromptFileProvider>,
    pub tool_packs: Vec<Arc<dyn ToolPack>>,
}

impl SessionHostServices {
    /// Build a minimal embeddable host surface from the required core services.
    ///
    /// Extension runtime, post-compact enrichment, and tool packs default to
    /// no-op/empty implementations so alternate hosts can opt in explicitly.
    pub fn embedded(
        context_assembler: Arc<dyn ContextAssembler>,
        prompt_provider: Arc<dyn PromptProvider>,
        prompt_file_provider: Arc<dyn PromptFileProvider>,
    ) -> Self {
        Self {
            extension_runner: Arc::new(NoopExtensionRuntime),
            context_assembler,
            post_compact_enricher: Arc::new(NoopPostCompactEnricher),
            prompt_provider,
            prompt_file_provider,
            tool_packs: Vec::new(),
        }
    }

    pub fn with_extension_runner(mut self, extension_runner: Arc<dyn ExtensionRuntime>) -> Self {
        self.extension_runner = extension_runner;
        self
    }

    pub fn with_post_compact_enricher(
        mut self,
        post_compact_enricher: Arc<dyn PostCompactEnricher>,
    ) -> Self {
        self.post_compact_enricher = post_compact_enricher;
        self
    }

    pub fn with_tool_packs(mut self, tool_packs: Vec<Arc<dyn ToolPack>>) -> Self {
        self.tool_packs = tool_packs;
        self
    }
}

struct ProviderSlot {
    provider: Arc<dyn LlmProvider>,
}

impl SessionRuntimeServices {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        effective_config: EffectiveConfig,
        host_services: SessionHostServices,
    ) -> Self {
        Self {
            llm: ArcSwap::from_pointee(ProviderSlot { provider: llm }),
            small_llm: ArcSwap::from_pointee(ProviderSlot {
                provider: small_llm,
            }),
            extension_runner: host_services.extension_runner,
            context_assembler: host_services.context_assembler,
            post_compact_enricher: host_services.post_compact_enricher,
            prompt_provider: host_services.prompt_provider,
            prompt_file_provider: host_services.prompt_file_provider,
            effective_config: ArcSwap::from_pointee(effective_config),
            tool_packs: Arc::from(host_services.tool_packs),
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

    pub fn extension_runner(&self) -> &dyn ExtensionRuntime {
        self.extension_runner.as_ref()
    }

    pub fn extension_runner_arc(&self) -> Arc<dyn ExtensionRuntime> {
        Arc::clone(&self.extension_runner)
    }

    pub fn context_assembler(&self) -> &dyn ContextAssembler {
        self.context_assembler.as_ref()
    }

    pub fn context_assembler_arc(&self) -> Arc<dyn ContextAssembler> {
        Arc::clone(&self.context_assembler)
    }

    pub fn post_compact_enricher(&self) -> &dyn PostCompactEnricher {
        self.post_compact_enricher.as_ref()
    }

    pub fn post_compact_enricher_arc(&self) -> Arc<dyn PostCompactEnricher> {
        Arc::clone(&self.post_compact_enricher)
    }

    pub fn prompt_provider(&self) -> &dyn PromptProvider {
        self.prompt_provider.as_ref()
    }

    pub fn prompt_provider_arc(&self) -> Arc<dyn PromptProvider> {
        Arc::clone(&self.prompt_provider)
    }

    pub fn prompt_file_provider(&self) -> &dyn PromptFileProvider {
        self.prompt_file_provider.as_ref()
    }

    pub fn prompt_file_provider_arc(&self) -> Arc<dyn PromptFileProvider> {
        Arc::clone(&self.prompt_file_provider)
    }

    pub fn tool_packs(&self) -> &[Arc<dyn ToolPack>] {
        &self.tool_packs
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        self.effective_config.load_full()
    }

    pub fn update_effective(&self, new: EffectiveConfig) {
        self.effective_config.store(Arc::new(new));
    }

    /// 获取 session_ops 能力引用（从 extension_runner 读取）。
    pub fn session_ops(&self) -> Option<Arc<dyn astrcode_core::tool::SessionOperations>> {
        self.extension_runner.session_ops()
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
        prompt::{PromptFiles, PromptPlan, SystemPromptInput},
        tool::ToolDefinition,
    };
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

    struct CustomPromptProvider;

    #[async_trait::async_trait]
    impl PromptProvider for CustomPromptProvider {
        async fn assemble(&self, _input: SystemPromptInput) -> PromptPlan {
            PromptPlan::from_system_prompt("custom prompt".into())
        }
    }

    struct CustomPromptFileProvider;

    #[async_trait::async_trait]
    impl PromptFileProvider for CustomPromptFileProvider {
        async fn load(&self, _working_dir: &str, include_agents_rules: bool) -> PromptFiles {
            PromptFiles {
                identity: Some("custom identity".into()),
                user_rules: include_agents_rules.then(|| "custom user rules".into()),
                project_rules: None,
            }
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
    async fn accepts_custom_context_and_prompt_services() {
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
            SessionHostServices::embedded(
                Arc::clone(&context_assembler),
                Arc::new(CustomPromptProvider),
                Arc::new(CustomPromptFileProvider),
            )
            .with_post_compact_enricher(Arc::new(CountingPostCompactEnricher)),
        );

        assert!(!services.context_assembler().auto_compact_enabled());
        let plan = services
            .prompt_provider()
            .assemble(system_prompt_input())
            .await;
        assert_eq!(plan.system_prompt.as_deref(), Some("custom prompt"));
        let files = services.prompt_file_provider().load(".", true).await;
        assert_eq!(files.identity.as_deref(), Some("custom identity"));
        assert_eq!(files.user_rules.as_deref(), Some("custom user rules"));
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

    fn system_prompt_input() -> SystemPromptInput {
        SystemPromptInput {
            working_dir: ".".into(),
            os: "test".into(),
            shell: "test".into(),
            gh_cli_available: false,
            identity: None,
            user_rules: None,
            project_rules: None,
            tools: Vec::new(),
            tool_prompt_metadata: Default::default(),
            extension_blocks: Vec::new(),
            extra_instructions: None,
        }
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
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
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
