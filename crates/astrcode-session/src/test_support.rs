use std::sync::Arc;

use astrcode_context::{
    CompactResult, ContextAssembler, ContextPrepareInput, NoopPostCompactEnricher,
    PostCompactEnrichInput, PostCompactEnricher, PreparedContext,
    context_assembler::LlmContextAssembler,
};
use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    event::{
        DurableEvent, DurableEventPayload, Event, PersistedSystemPrompt, SessionStarted,
        StoredEvent, SystemPromptSource,
    },
    llm::{LlmError, LlmEvent, LlmProvider, LlmRequest, ModelLimits},
    tool::SessionToolSelection,
    types::SessionId,
};
use astrcode_extension_sdk::runtime_ports::{NoopRuntimePorts, TurnHooks};
use astrcode_session_projection::{SessionReadModel, replay};
use tokio::sync::mpsc;

pub(crate) fn read_model(session_id: SessionId) -> SessionReadModel {
    let started = DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: "/workspace".into(),
            model_id: "model".into(),
            parent: None,
            tool_selection: SessionToolSelection::default(),
            source_extension: None,
            initial_system_prompt: PersistedSystemPrompt {
                text: "system".into(),
                fingerprint: "fingerprint".into(),
                extra_system_prompt: None,
                source: SystemPromptSource::Native,
            },
        }),
    );

    replay(session_id, &[StoredEvent::new(0, started)]).unwrap()
}

/// LLM provider 桩：测试不真正调用 LLM，任何调用都是测试设计的错误。
pub(crate) struct UnusedLlm;

#[async_trait::async_trait]
impl LlmProvider for UnusedLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        unreachable!("test does not call the LLM")
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

/// 统一的 mock LlmSettings，避免各测试重复完整字段字面量。
pub(crate) fn test_llm_settings() -> LlmSettings {
    LlmSettings {
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
        thinking: Default::default(),
        thinking_capability: None,
        thinking_configured: false,
    }
}

/// 统一的 mock EffectiveConfig（主/小模型同用一份 mock LlmSettings）。
pub(crate) fn test_effective_config(context: ContextSettings) -> EffectiveConfig {
    let llm = test_llm_settings();
    EffectiveConfig {
        llm: llm.clone(),
        small_llm: llm,
        context,
        agent: AgentSettings::default(),
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    }
}

/// 不触发 auto-compact 的 context assembler 桩。
pub(crate) struct NoopContextAssembler {
    settings: ContextSettings,
}

impl NoopContextAssembler {
    pub(crate) fn new(settings: ContextSettings) -> Self {
        Self { settings }
    }
}

impl ContextAssembler for NoopContextAssembler {
    fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    fn should_auto_compact(&self, _input: &ContextPrepareInput<'_>) -> bool {
        false
    }

    fn prepare_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext {
        LlmContextAssembler::new(self.settings.clone()).prepare_messages(input)
    }
}

/// 追加标记文本的 post-compact enricher 桩。
pub(crate) struct CountingPostCompactEnricher;

#[async_trait::async_trait]
impl PostCompactEnricher for CountingPostCompactEnricher {
    async fn enrich(&self, compaction: &mut CompactResult, _input: PostCompactEnrichInput<'_>) {
        compaction.summary.push_str(" enriched");
    }
}

/// 事件观察者桩：转发到 mpsc 通道供断言。
pub(crate) struct ChannelObserver(mpsc::UnboundedSender<Arc<Event>>);

impl ChannelObserver {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Arc<Event>>) -> Arc<Self> {
        Arc::new(Self(tx))
    }
}

impl crate::SessionEventObserver for ChannelObserver {
    fn publish(&self, event: Arc<Event>) {
        let _ = self.0.send(event);
    }
}

/// 用 mock LLM + mock 配置构建 runtime services（不触发 auto-compact）。
pub(crate) fn test_runtime_services() -> Arc<crate::SessionRuntimeServices> {
    test_runtime_services_with_hooks(Arc::new(NoopRuntimePorts))
}

/// 用 mock LLM + mock 配置 + 自定义 hooks 构建 runtime services。
pub(crate) fn test_runtime_services_with_hooks(
    turn_hooks: Arc<dyn TurnHooks>,
) -> Arc<crate::SessionRuntimeServices> {
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    Arc::new(crate::SessionRuntimeServices::new_with_context_assembler(
        llm.clone(),
        llm,
        test_effective_config(ContextSettings::default()),
        crate::SessionExtensionPorts::with_turn_hooks(turn_hooks),
        Arc::new(NoopContextAssembler::new(ContextSettings::default())),
        Arc::new(NoopPostCompactEnricher),
    ))
}
