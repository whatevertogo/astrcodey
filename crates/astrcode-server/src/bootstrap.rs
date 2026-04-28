//! Server bootstrap — assembles all services from config.

use std::{sync::Arc, time::Duration};

use astrcode_ai::openai::OpenAiProvider;
use astrcode_context::{
    budget::ToolResultBudget, file_access::FileAccessTracker, settings::ContextWindowSettings,
};
use astrcode_core::{
    config::{ConfigStore, EffectiveConfig},
    llm::{LlmClientConfig, LlmProvider},
    prompt::PromptProvider,
};
use astrcode_extensions::{
    loader::ExtensionLoader,
    runner::ExtensionRunner,
    runtime::{SessionSpawner, SpawnRequest, SpawnResult},
};
use astrcode_storage::config_store::FileConfigStore;
use astrcode_tools::registry::ToolRegistry;

use crate::{agent::Agent, capability::CapabilityRouter, session::SessionManager};

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// All services assembled at startup. Grouped by domain.
pub struct ServerRuntime {
    /// Core services
    pub session_manager: Arc<SessionManager>,
    pub llm_provider: Arc<dyn LlmProvider>,
    pub prompt_provider: Arc<dyn PromptProvider>,
    pub capability: Arc<CapabilityRouter>,
    /// Extension system
    pub extension_runner: Arc<ExtensionRunner>,
    /// Resolved config (read-only)
    pub effective: EffectiveConfig,
    /// Context window management
    pub context_settings: ContextWindowSettings,
    pub tool_result_budget: Arc<ToolResultBudget>,
    pub file_access_tracker: Arc<std::sync::Mutex<FileAccessTracker>>,
}

// ─── Bootstrap ───────────────────────────────────────────────────────────

/// Bootstrap options for testability.
#[derive(Default)]
pub struct BootstrapOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub working_dir: Option<std::path::PathBuf>,
}

pub async fn bootstrap() -> Result<ServerRuntime, BootstrapError> {
    bootstrap_with(BootstrapOptions::default()).await
}

pub async fn bootstrap_with(opts: BootstrapOptions) -> Result<ServerRuntime, BootstrapError> {
    // 1. Load + resolve config
    let config_store = if let Some(ref path) = opts.config_path {
        FileConfigStore::new(path.clone())
    } else {
        FileConfigStore::default_path()
    };
    let config = config_store.load().await?;
    let effective = config.into_effective()?;

    // 2. Build LLM provider
    let llm_config = LlmClientConfig {
        base_url: effective.llm.base_url.clone(),
        api_key: effective.llm.api_key.clone(),
        connect_timeout_secs: effective.llm.connect_timeout_secs,
        read_timeout_secs: effective.llm.read_timeout_secs,
        max_retries: effective.llm.max_retries,
        retry_base_delay_ms: effective.llm.retry_base_delay_ms,
        extra_headers: Default::default(),
    };
    let llm_provider: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    ));

    // 3. Build prompt provider
    let mut composer = astrcode_prompt::composer::PromptComposer::new();
    composer.add_contributor(Box::new(astrcode_prompt::contributors::IdentityContributor));
    composer.add_contributor(Box::new(
        astrcode_prompt::contributors::EnvironmentContributor,
    ));
    composer.add_contributor(Box::new(astrcode_prompt::contributors::AgentsMdContributor));
    composer.add_contributor(Box::new(
        astrcode_prompt::contributors::CapabilityContributor,
    ));
    composer.add_contributor(Box::new(
        astrcode_prompt::contributors::ResponseStyleContributor,
    ));
    composer.add_contributor(Box::new(
        astrcode_prompt::contributors::SystemInstructionContributor,
    ));
    let prompt_provider: Arc<dyn PromptProvider> = Arc::new(composer);

    // 4. Build capability router with stable built-in tools
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register_builtins(cwd.clone(), effective.llm.read_timeout_secs);

    let capability = Arc::new(CapabilityRouter::new());
    for tool in tool_registry.into_tools() {
        capability.register_stable(tool).await;
    }

    // 5. Session manager with storage backend
    let project_hash = astrcode_core::types::project_hash_from_path(&cwd);
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        // Test path → use memory-only store
        Arc::new(astrcode_storage::noop::NoopEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new(project_hash))
    };
    let session_manager = Arc::new(SessionManager::new(store));

    // 6. Extension runner — load from disk then bind core services
    let cwd_str = cwd.to_string_lossy().to_string();
    let load_result = ExtensionLoader::load_all(Some(&cwd_str)).await;
    let extension_runner = Arc::new(ExtensionRunner::new(
        Duration::from_secs(30),
        load_result.runtime,
    ));
    for ext in load_result.extensions {
        extension_runner.register(ext).await;
    }
    for err in &load_result.errors {
        tracing::warn!("Extension load error: {err}");
    }
    let extension_tools = extension_runner.collect_tool_adapters(&cwd_str).await;
    if !extension_tools.is_empty() {
        capability.apply_dynamic(extension_tools).await;
    }

    // Bind session spawn capability so extensions can request RunSession outcomes.
    extension_runner.bind(Arc::new(ServerSessionSpawner {
        session_manager: Arc::clone(&session_manager),
        llm: Arc::clone(&llm_provider),
        capability: Arc::clone(&capability),
        prompt: Arc::clone(&prompt_provider),
        extension_runner: Arc::clone(&extension_runner),
    }));

    // 7. Context window management
    let context_settings = ContextWindowSettings::default();
    let tool_result_budget = Arc::new(ToolResultBudget::new(
        context_settings.summary_reserve_tokens * 3, // aggregate
        context_settings.max_tracked_files * 1024,   // inline
        context_settings.recovery_token_budget * 3,  // preview
    ));
    let file_access_tracker = Arc::new(std::sync::Mutex::new(FileAccessTracker::new(
        context_settings.max_tracked_files,
    )));

    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        prompt_provider,
        capability,
        extension_runner,
        effective,
        context_settings,
        tool_result_budget,
        file_access_tracker,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
    #[error("Resolve: {0}")]
    Resolve(#[from] astrcode_core::config::ResolveError),
}

// ─── ServerSessionSpawner ─────────────────────────────────────────────────

/// Implements `SessionSpawner` so the extension runner can spawn child sessions
/// when extensions return `ExtensionToolOutcome::RunSession`.
struct ServerSessionSpawner {
    session_manager: Arc<SessionManager>,
    llm: Arc<dyn LlmProvider>,
    capability: Arc<CapabilityRouter>,
    prompt: Arc<dyn PromptProvider>,
    extension_runner: Arc<ExtensionRunner>,
}

#[async_trait::async_trait]
impl SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let model_id = request
            .model_preference
            .clone()
            .unwrap_or_else(|| "default".into());

        let create_event = self
            .session_manager
            .create(
                &request.working_dir,
                &model_id,
                2048,
                Some(parent_session_id),
            )
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let child_sid = create_event.session_id.to_string();

        let agent = Agent::new(
            child_sid.clone(),
            request.working_dir,
            Arc::clone(&self.llm),
            Arc::clone(&self.prompt),
            Arc::clone(&self.capability),
            Arc::clone(&self.extension_runner),
            model_id,
            8192,
        )
        .with_system_prompt_suffix(request.system_prompt)
        .with_tool_allowlist(request.allowed_tools);

        match agent
            .process_prompt(&request.user_prompt, Vec::new(), None)
            .await
        {
            Ok(output) => Ok(SpawnResult {
                content: output.text,
                child_session_id: child_sid,
            }),
            Err(e) => Ok(SpawnResult {
                content: format!("child agent error: {e}"),
                child_session_id: child_sid,
            }),
        }
    }
}
