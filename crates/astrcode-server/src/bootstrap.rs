//! Server bootstrap — assembles all services from config.

use std::sync::Arc;
use std::time::Duration;

use astrcode_ai::openai::OpenAiProvider;
use astrcode_core::config::{ConfigStore, EffectiveConfig};
use astrcode_core::llm::{LlmClientConfig, LlmProvider};
use astrcode_core::prompt::PromptProvider;
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_storage::config_store::FileConfigStore;
use astrcode_tools::registry::ToolRegistry;

use crate::capability::CapabilityRouter;
use crate::session::SessionManager;

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
}

// ─── Bootstrap ───────────────────────────────────────────────────────────

/// Bootstrap options for testability.
pub struct BootstrapOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub working_dir: Option<std::path::PathBuf>,
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            working_dir: None,
        }
    }
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
    tool_registry.register_builtins(cwd, effective.llm.read_timeout_secs);

    let capability = Arc::new(CapabilityRouter::new());
    for tool in tool_registry.into_tools() {
        capability.register_stable(tool).await;
    }

    // 5. Session manager
    let session_manager = Arc::new(SessionManager::new());

    // 6. Extension runner
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));

    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        prompt_provider,
        capability,
        extension_runner,
        effective,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
    #[error("Resolve: {0}")]
    Resolve(#[from] astrcode_core::config::ResolveError),
}
