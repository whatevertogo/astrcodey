use std::sync::Arc;

use astrcode_context::{
    ContextAssembler, ContextPrepareInput, NoopPostCompactEnricher, PreparedContext,
    context_assembler::LlmContextAssembler,
};
use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    llm::LlmProvider,
};
use astrcode_extension_sdk::runtime_ports::{NoopRuntimePorts, ToolCatalogProvider};
use astrcode_session::{SessionExtensionPorts, SessionRuntimeServices};

pub fn test_runtime_services(llm: Arc<dyn LlmProvider>) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context(llm, ContextSettings::default())
}

pub fn test_runtime_services_with_context(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context_and_extensions(
        llm,
        context,
        SessionExtensionPorts::default(),
        None,
    )
}

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub fn test_runtime_services_with_extensions(
    llm: Arc<dyn LlmProvider>,
    extension_ports: SessionExtensionPorts,
) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context_and_extensions(
        llm,
        ContextSettings::default(),
        extension_ports,
        None,
    )
}

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub fn test_runtime_services_with_tool_catalog(
    llm: Arc<dyn LlmProvider>,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context_and_extensions(
        llm,
        ContextSettings::default(),
        SessionExtensionPorts::default(),
        Some(tool_catalog),
    )
}

fn test_runtime_services_with_context_and_extensions(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
    extension_ports: SessionExtensionPorts,
    tool_catalog: Option<Arc<dyn ToolCatalogProvider>>,
) -> Arc<SessionRuntimeServices> {
    let context_assembler: Arc<dyn ContextAssembler> = Arc::new(NoopContextAssembler {
        settings: context.clone(),
    });
    let tool_catalog =
        tool_catalog.unwrap_or_else(|| Arc::new(NoopRuntimePorts) as Arc<dyn ToolCatalogProvider>);
    Arc::new(SessionRuntimeServices::new(
        llm.clone(),
        llm,
        effective_config(context),
        extension_ports,
        context_assembler,
        Arc::new(NoopPostCompactEnricher),
        tool_catalog,
    ))
}

struct NoopContextAssembler {
    settings: ContextSettings,
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
