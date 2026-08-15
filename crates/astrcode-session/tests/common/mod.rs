use std::sync::Arc;

use astrcode_context::{
    ContextAssembler, ContextPrepareInput, PreparedContext, context_assembler::LlmContextAssembler,
};
use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    llm::LlmProvider,
    types::{SessionId, new_session_id},
};
use astrcode_extension_sdk::runtime_ports::{NoopRuntimePorts, ToolCatalogProvider};
use astrcode_session::{
    Session, SessionCreateParams, SessionExtensionPorts, SessionRuntimeServices,
    SessionRuntimeState,
};
use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub fn test_runtime_services(llm: Arc<dyn LlmProvider>) -> Arc<SessionRuntimeServices> {
    test_runtime_services_with_context(llm, ContextSettings::default())
}

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
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

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub async fn spawn_session(llm: Arc<dyn LlmProvider>) -> Session {
    spawn_session_with_context_and_services(llm, ContextSettings::default())
        .await
        .0
}

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub async fn spawn_session_with_store(
    llm: Arc<dyn LlmProvider>,
) -> (Session, Arc<dyn SessionStore>, SessionId) {
    let (session, store, sid, _) =
        spawn_session_with_context_and_services(llm, ContextSettings::default()).await;
    (session, store, sid)
}

#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub async fn spawn_session_with_services(
    llm: Arc<dyn LlmProvider>,
) -> (
    Session,
    Arc<dyn SessionStore>,
    SessionId,
    Arc<SessionRuntimeServices>,
) {
    spawn_session_with_context_and_services(llm, ContextSettings::default()).await
}

/// 与 [`spawn_session_with_context_and_services`] 相同,但使用真实的
/// [`LlmContextAssembler`](按 token 阈值判定),而非测试用的恒真判定。
#[allow(dead_code)] // Each integration-test binary imports this shared module independently.
pub async fn spawn_session_with_llm_assembler(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
) -> (Session, Arc<dyn SessionStore>, SessionId) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let context_assembler: Arc<dyn ContextAssembler> =
        Arc::new(LlmContextAssembler::new(context.clone()));
    let caps = Arc::new(SessionRuntimeServices::new_with_context_assembler(
        llm.clone(),
        llm,
        effective_config(context),
        SessionExtensionPorts::default(),
        context_assembler,
    ));
    let sid = new_session_id();
    let runtime = Arc::new(SessionRuntimeState::new(sid.clone(), store.clone()));
    let working_dir = std::env::temp_dir().join(sid.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    let session = Session::create_with_params(SessionCreateParams {
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime,
        runtime_services: caps,
    })
    .await
    .unwrap();
    (session, store, sid)
}

pub async fn spawn_session_with_context_and_services(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
) -> (
    Session,
    Arc<dyn SessionStore>,
    SessionId,
    Arc<SessionRuntimeServices>,
) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_runtime_services_with_context(llm, context);
    let sid = new_session_id();
    let runtime = Arc::new(SessionRuntimeState::new(sid.clone(), store.clone()));
    let working_dir = std::env::temp_dir().join(sid.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    let session = Session::create_with_params(SessionCreateParams {
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime,
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();
    (session, store, sid, caps)
}

fn test_runtime_services_with_context_and_extensions(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
    extension_ports: SessionExtensionPorts,
    tool_catalog: Option<Arc<dyn ToolCatalogProvider>>,
) -> Arc<SessionRuntimeServices> {
    let context_assembler: Arc<dyn ContextAssembler> = Arc::new(TestContextAssembler {
        settings: context.clone(),
    });
    let extension_ports = if let Some(tool_catalog) = tool_catalog {
        let noop = Arc::new(NoopRuntimePorts);
        SessionExtensionPorts::from_immutable_ports(tool_catalog, noop.clone(), noop.clone(), noop)
    } else {
        extension_ports
    };
    Arc::new(SessionRuntimeServices::new_with_context_assembler(
        llm.clone(),
        llm,
        effective_config(context),
        extension_ports,
        context_assembler,
    ))
}

struct TestContextAssembler {
    settings: ContextSettings,
}

impl ContextAssembler for TestContextAssembler {
    fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool {
        self.settings.auto_compact_enabled && !input.messages.is_empty()
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
