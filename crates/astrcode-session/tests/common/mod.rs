use std::sync::Arc;

use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    context::{
        CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn,
        CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput,
    },
    llm::{LlmMessage, LlmProvider},
    prompt::{PromptFileProvider, PromptFiles, PromptPlan, PromptProvider, SystemPromptInput},
};
use astrcode_session::{SessionHostServices, SessionRuntimeServices};

pub fn test_runtime_services(llm: Arc<dyn LlmProvider>) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context(llm, ContextSettings::default())
}

pub fn test_runtime_services_with_context(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
) -> Arc<SessionRuntimeServices> {
    let context_assembler: Arc<dyn ContextAssembler> = Arc::new(NoopContextAssembler {
        settings: context.clone(),
    });
    Arc::new(SessionRuntimeServices::new(
        llm.clone(),
        llm,
        effective_config(context),
        SessionHostServices::embedded(
            context_assembler,
            Arc::new(StaticPromptProvider),
            Arc::new(StaticPromptFileProvider),
        ),
    ))
}

struct NoopContextAssembler {
    settings: ContextSettings,
}

#[async_trait::async_trait]
impl ContextAssembler for NoopContextAssembler {
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

struct StaticPromptProvider;

#[async_trait::async_trait]
impl PromptProvider for StaticPromptProvider {
    async fn assemble(&self, input: SystemPromptInput) -> PromptPlan {
        PromptPlan::from_system_prompt(format!(
            "[Identity]\n  test host\n\n[Environment]\n  Working directory: {}\nOS: {}\nShell: {}",
            input.working_dir, input.os, input.shell
        ))
    }
}

struct StaticPromptFileProvider;

#[async_trait::async_trait]
impl PromptFileProvider for StaticPromptFileProvider {
    async fn load(&self, _working_dir: &str, _include_agents_rules: bool) -> PromptFiles {
        PromptFiles::default()
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
        context_limit: 200_000,
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
