//! Session 跨实例恢复时 extra_system_prompt 不丢失。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::EventStore,
    tool::ToolDefinition,
    types::new_session_id,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{Session, SessionRuntimeServices, SessionRuntimeState};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

struct UnusedLlm;

#[async_trait::async_trait]
impl LlmProvider for UnusedLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        unreachable!("test does not run a turn")
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

fn test_caps() -> Arc<SessionRuntimeServices> {
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let context_assembler = Arc::new(LlmContextAssembler::new(ContextSettings::default()));
    let effective = EffectiveConfig {
        llm: LlmSettings {
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
            temperature: None,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        small_llm: LlmSettings {
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
            temperature: None,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        context: ContextSettings::default(),
        agent: astrcode_core::config::AgentSettings::default(),
        wasm: astrcode_core::config::WasmSettings::default(),
        extensions: ExtensionSettings::default(),
    };
    Arc::new(SessionRuntimeServices::new(
        llm.clone(),
        llm,
        extension_runner,
        context_assembler,
        effective,
    ))
}

#[tokio::test]
async fn refresh_prompt_with_none_preserves_existing_extra() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps();
    let sid = new_session_id();

    // 第一次 — 模拟子会话首次 spawn：runtime 注入 extra，refresh_prompt 显式传入
    let runtime_a = Arc::new(SessionRuntimeState::default());
    runtime_a.set_extra_system_prompt(Some("child agent body".into()));
    let session_a = Session::create_with_id(
        Arc::clone(&store),
        sid.clone(),
        ".",
        "mock-model",
        None,
        None,
        None,
        Arc::clone(&runtime_a),
        Arc::clone(&caps),
    )
    .await
    .unwrap();
    session_a.refresh_tools(".").await;
    let wrote_a = session_a
        .refresh_prompt(".", Some("child agent body"), None)
        .await
        .expect("first refresh_prompt should succeed");
    assert!(wrote_a, "first refresh should write SystemPromptConfigured");

    let state_after_first = session_a.read_model().await.unwrap();
    assert_eq!(
        state_after_first.extra_system_prompt.as_deref(),
        Some("child agent body"),
    );

    // 模拟跨进程重启：丢弃 runtime_a，开新 runtime + Session 实例
    drop(session_a);
    drop(runtime_a);
    let runtime_b = Arc::new(SessionRuntimeState::default());
    assert!(runtime_b.extra_system_prompt().is_none());
    let session_b = Session::open(
        Arc::clone(&store),
        sid.clone(),
        Arc::clone(&runtime_b),
        Arc::clone(&caps),
    )
    .await
    .unwrap();
    session_b.refresh_tools(".").await;

    // handler 风格的调用 — extra=None，期望「保留」从 projection 恢复
    let stored_fp = state_after_first.system_prompt_fingerprint.clone();
    let wrote_b = session_b
        .refresh_prompt(".", None, stored_fp.as_deref())
        .await
        .expect("second refresh_prompt should succeed");
    assert!(
        !wrote_b,
        "fingerprint hit should skip writing a new SystemPromptConfigured event",
    );

    // 关键断言：projection 仍然带着 extra；runtime 被恢复
    let state_after_second = session_b.read_model().await.unwrap();
    assert_eq!(
        state_after_second.extra_system_prompt.as_deref(),
        Some("child agent body"),
        "extra_system_prompt must survive refresh_prompt(None) on a reopened session",
    );
    assert_eq!(
        runtime_b.extra_system_prompt().as_deref(),
        Some("child agent body"),
        "runtime_b should be hydrated from projection",
    );
}
