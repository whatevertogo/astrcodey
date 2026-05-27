//! 集成测试用进程内 runtime（Mock LLM、内存 EventStore、不加载磁盘/MCP 扩展）。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::EventStore,
    tool::ToolDefinition,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::{config_store::FileConfigStore, in_memory::InMemoryEventStore};
use tokio::sync::mpsc;

use crate::{
    bootstrap::ServerRuntime, config_manager::ConfigManager, session_manager::SessionManager,
};

/// 快速返回固定 assistant 文本的 mock provider。
struct MockLlm;

#[async_trait::async_trait]
impl LlmProvider for MockLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "mock-e2e-response".into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

fn mock_llm_settings() -> LlmSettings {
    LlmSettings {
        provider_kind: "mock".into(),
        base_url: String::new(),
        api_key: String::new(),
        api_mode: OpenAiApiMode::ChatCompletions,
        model_id: "mock-model".into(),
        max_tokens: 1024,
        context_limit: 1024,
        connect_timeout_secs: 1,
        read_timeout_secs: 1,
        max_retries: 0,
        retry_base_delay_ms: 0,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        reasoning: false,
        thinking_level: None,
    }
}

/// 供 CLI 进程内集成测试使用的轻量 [`ServerRuntime`]。
pub fn in_process_test_runtime() -> Arc<ServerRuntime> {
    let llm = Arc::new(MockLlm) as Arc<dyn LlmProvider>;
    let effective = EffectiveConfig {
        llm: mock_llm_settings(),
        small_llm: mock_llm_settings(),
        context: ContextSettings::default(),
        agent: AgentSettings::default(),
        extensions: ExtensionSettings::default(),
    };
    let event_store = Arc::new(InMemoryEventStore::new()) as Arc<dyn EventStore>;
    let extension_runner = Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
        Duration::from_secs(1),
    ));
    let context_assembler = Arc::new(LlmContextAssembler::new(ContextSettings::default()));
    let capabilities = Arc::new(SessionRuntimeServices::new(
        Arc::clone(&llm),
        llm,
        Arc::clone(&extension_runner),
        Arc::clone(&context_assembler),
        effective,
    ));
    let config_manager = Arc::new(ConfigManager::new(
        Arc::new(FileConfigStore::new(std::path::PathBuf::from(
            "target/in-process-test-config.json",
        ))),
        astrcode_core::config::Config::default(),
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config_manager),
        Arc::clone(&capabilities),
        vec![],
    ));
    Arc::new(ServerRuntime::assemble_for_test(
        event_store,
        config_manager,
        context_assembler,
        session_manager,
        extension_runner,
        capabilities,
        std::env::temp_dir(),
    ))
}
