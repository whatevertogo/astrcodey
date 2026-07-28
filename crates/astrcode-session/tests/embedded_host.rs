use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    context::{
        CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn,
        CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    prompt::{PromptFileProvider, PromptFiles, PromptPlan, PromptProvider, SystemPromptInput},
    storage::EventStore,
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        ToolResult,
    },
    types::new_session_id,
};
use astrcode_extension_sdk::{
    runtime_ports::{NoopRuntimePorts, ToolCatalogProvider, ToolCatalogSnapshot},
    tool_pack::{ToolPack, ToolPackScope},
};
use astrcode_session::{
    Session, SessionCreateParams, SessionExtensionPorts, SessionHostServices,
    SessionRuntimeServices, SessionRuntimeState,
};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

struct EmbeddedLlm;

#[async_trait::async_trait]
impl LlmProvider for EmbeddedLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        unreachable!("embedded host smoke test only initializes the session")
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 4096,
            max_output_tokens: 512,
        }
    }
}

struct EmbeddedContextAssembler {
    settings: ContextSettings,
}

#[async_trait::async_trait]
impl ContextAssembler for EmbeddedContextAssembler {
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

struct EmbeddedPromptProvider;

#[async_trait::async_trait]
impl PromptProvider for EmbeddedPromptProvider {
    async fn assemble(&self, input: SystemPromptInput) -> PromptPlan {
        PromptPlan::from_system_prompt(format!(
            "embedded identity: {}\nembedded project rules: {}\nembedded tools: {}",
            input.identity.unwrap_or_default(),
            input.project_rules.unwrap_or_default(),
            input
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

struct EmbeddedPromptFiles;

#[async_trait::async_trait]
impl PromptFileProvider for EmbeddedPromptFiles {
    async fn load(&self, _working_dir: &str, include_agents_rules: bool) -> PromptFiles {
        PromptFiles {
            identity: Some("memory-host".into()),
            user_rules: None,
            project_rules: include_agents_rules.then(|| "memory-project-rules".into()),
        }
    }
}

struct EmbeddedToolPack {
    calls: Arc<AtomicUsize>,
}
struct EmbeddedToolCatalog {
    calls: Arc<AtomicUsize>,
}
struct EmbeddedEchoTool {
    name: &'static str,
    origin: ToolOrigin,
}

impl ToolPack for EmbeddedToolPack {
    fn tools(&self, _scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        vec![Arc::new(EmbeddedEchoTool {
            name: "embeddedEcho",
            origin: ToolOrigin::Sdk,
        })]
    }
}

#[async_trait::async_trait]
impl ToolCatalogProvider for EmbeddedToolCatalog {
    async fn tool_catalog(
        &self,
        _working_dir: &str,
    ) -> Result<ToolCatalogSnapshot, astrcode_core::extension::ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCatalogSnapshot::complete(vec![
            Arc::new(EmbeddedEchoTool {
                name: "embeddedEcho",
                origin: ToolOrigin::Extension,
            }),
            Arc::new(EmbeddedEchoTool {
                name: "catalogOnly",
                origin: ToolOrigin::Extension,
            }),
        ]))
    }
}

#[async_trait::async_trait]
impl Tool for EmbeddedEchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Echoes an embedded host value.".into(),
            parameters: serde_json::json!({"type": "object"}),
            strict: false,
            origin: self.origin,
            execution_mode: ExecutionMode::Sequential,
        }
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text(
            "embedded".into(),
            false,
            Default::default(),
        ))
    }
}

#[tokio::test]
async fn embedded_host_initializes_session_with_custom_services() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(EmbeddedLlm);
    let tool_pack_calls = Arc::new(AtomicUsize::new(0));
    let tool_catalog_calls = Arc::new(AtomicUsize::new(0));
    let noop = Arc::new(NoopRuntimePorts);
    let extension_ports = SessionExtensionPorts::from_immutable_ports(
        Arc::new(EmbeddedToolCatalog {
            calls: Arc::clone(&tool_catalog_calls),
        }),
        noop.clone(),
        noop.clone(),
        noop,
    );
    let caps = Arc::new(SessionRuntimeServices::new(
        Arc::clone(&llm),
        llm,
        effective_config(),
        SessionHostServices::embedded(
            Arc::new(EmbeddedContextAssembler {
                settings: ContextSettings::default(),
            }),
            Arc::new(EmbeddedPromptProvider),
            Arc::new(EmbeddedPromptFiles),
        )
        .with_extension_ports(extension_ports)
        .with_tool_packs(vec![Arc::new(EmbeddedToolPack {
            calls: Arc::clone(&tool_pack_calls),
        })]),
    ));
    let runtime = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "embedded-model".into(),
    ));
    let session = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        session_id: new_session_id(),
        working_dir: "memory://workspace".into(),
        model_id: "embedded-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: Some("embedded-host-test".into()),
        runtime,
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    let (registry, cached_registry) = tokio::join!(
        session.tool_registry_snapshot("memory://workspace"),
        session.tool_registry_snapshot("memory://workspace"),
    );
    let registry = registry.unwrap();
    let cached_registry = cached_registry.unwrap();
    assert!(Arc::ptr_eq(&registry, &cached_registry));
    assert_eq!(tool_pack_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_catalog_calls.load(Ordering::SeqCst), 1);

    session
        .initialize_runtime("memory://workspace")
        .await
        .unwrap();
    session
        .configure_tools(astrcode_core::extension::SessionToolSelection::Only {
            names: vec!["embeddedEcho".into()],
        })
        .await
        .unwrap();
    let filtered_registry = session
        .tool_registry_snapshot("memory://workspace")
        .await
        .unwrap();
    let cached_filtered_registry = session
        .tool_registry_snapshot("memory://workspace")
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&filtered_registry, &cached_filtered_registry));
    assert_eq!(
        filtered_registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>(),
        ["embeddedEcho"],
    );
    assert_eq!(tool_pack_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_catalog_calls.load(Ordering::SeqCst), 1);

    let tool_names = registry
        .list_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["catalogOnly", "embeddedEcho"]);
    assert_eq!(
        registry.find_definition("embeddedEcho").unwrap().origin,
        ToolOrigin::Extension,
        "extension tools must override host-pack tools with the same name",
    );

    let model = store.session_read_model(session.id()).await.unwrap();
    let system_prompt = model.system_prompt.unwrap();
    assert!(system_prompt.contains("memory-host"));
    assert!(system_prompt.contains("memory-project-rules"));
    assert!(system_prompt.contains("embeddedEcho"));
}

fn effective_config() -> EffectiveConfig {
    let llm = LlmSettings {
        provider_kind: "embedded".into(),
        base_url: String::new(),
        api_key: String::new(),
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        model_id: "embedded-model".into(),
        max_tokens: 512,
        context_limit: 4096,
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
        context: ContextSettings::default(),
        agent: AgentSettings {
            shell_timeout_secs: Duration::from_secs(1).as_secs(),
            ..AgentSettings::default()
        },
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    }
}
