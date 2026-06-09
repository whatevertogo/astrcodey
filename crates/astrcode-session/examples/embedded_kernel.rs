use std::{error::Error, sync::Arc, time::Duration};

use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        OpenAiApiMode,
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
use astrcode_kernel::{ToolPack, ToolPackScope};
use astrcode_session::{Session, SessionHostServices, SessionRuntimeServices, SessionRuntimeState};
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
        Err(LlmError::Transport(
            "embedded example initializes the runtime without calling the provider".into(),
        ))
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

struct EmbeddedToolPack;
struct EmbeddedEchoTool;

impl ToolPack for EmbeddedToolPack {
    fn tools(&self, _scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(EmbeddedEchoTool)]
    }
}

#[async_trait::async_trait]
impl Tool for EmbeddedEchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "embeddedEcho".into(),
            description: "Echoes an embedded host value.".into(),
            parameters: serde_json::json!({"type": "object"}),
            origin: ToolOrigin::Sdk,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(EmbeddedLlm);
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
        .with_tool_packs(vec![Arc::new(EmbeddedToolPack)]),
    ));
    let runtime = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "embedded-model".into(),
    ));
    let session = Session::create_with_id(
        Arc::clone(&store),
        new_session_id(),
        "memory://workspace",
        "embedded-model",
        None,
        None,
        Some("embedded-kernel-example"),
        runtime,
        Arc::clone(&caps),
    )
    .await?;

    session.initialize_runtime("memory://workspace").await?;

    let registry = session.runtime().loaded_tool_registry();
    let tool_names = registry
        .list_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    if tool_names != vec!["embeddedEcho"] {
        return Err(std::io::Error::other("embedded tool pack was not registered").into());
    }

    let model = store.session_read_model(session.id()).await?;
    let system_prompt = model
        .system_prompt
        .ok_or_else(|| std::io::Error::other("system prompt was not configured"))?;
    for expected in ["memory-host", "memory-project-rules", "embeddedEcho"] {
        if !system_prompt.contains(expected) {
            return Err(
                std::io::Error::other(format!("system prompt did not contain {expected}")).into(),
            );
        }
    }

    Ok(())
}

fn effective_config() -> EffectiveConfig {
    let llm = LlmSettings {
        provider_kind: "embedded".into(),
        base_url: String::new(),
        api_key: String::new(),
        api_mode: OpenAiApiMode::ChatCompletions,
        model_id: "embedded-model".into(),
        max_tokens: 512,
        context_limit: 4096,
        connect_timeout_secs: 1,
        read_timeout_secs: 1,
        max_retries: 0,
        retry_base_delay_ms: 0,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        reasoning: false,
        thinking_level: None,
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
