//! 集成测试：ServerSessionOperations 的 submit_turn 同步/异步路径。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::{AgentSessionStatus, EventStore},
    tool::{
        CreateSessionRequest, SessionOperations, SubmitTurnRequest, SubmitTurnResult,
        ToolDefinition,
    },
    types::new_session_id,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_server::{
    server_event_bus::ServerEventBus, session_manager::SessionManager,
    session_operations::ServerSessionOperations, turn_registry::TurnRegistry,
    turn_scheduler::TurnScheduler,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::in_memory::InMemoryEventStore;
use astrcode_support::event_fanout::EventFanout;
use tokio::sync::mpsc;

struct StaticTextLlm {
    text: &'static str,
}

#[async_trait::async_trait]
impl LlmProvider for StaticTextLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: self.text.into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200000,
            max_output_tokens: 1024,
        }
    }
}

fn build_test_ops(
    store: Arc<dyn EventStore>,
    llm_text: &'static str,
) -> Arc<ServerSessionOperations> {
    let llm_provider: Arc<dyn LlmProvider> = Arc::new(StaticTextLlm { text: llm_text });
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let context_assembler = Arc::new(LlmContextAssembler::new(Default::default()));
    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
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
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        context: Default::default(),
        agent: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    let capabilities = Arc::new(SessionRuntimeServices::new(
        llm_provider.clone(),
        llm_provider,
        Arc::clone(&extension_runner),
        context_assembler,
        effective,
    ));
    let config = Arc::new(astrcode_server::config_manager::ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/test-session-ops-config.json"),
        )),
        Default::default(),
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        config,
        capabilities,
        vec![],
    ));
    let scheduler = Arc::new(TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(TurnRegistry::new()),
    ));
    let event_bus = Arc::new(ServerEventBus::new(
        Arc::new(EventFanout::new(1024)),
        Arc::clone(&scheduler),
    ));
    session_manager.bind_event_bus(event_bus);
    Arc::new(ServerSessionOperations {
        session_manager,
        scheduler,
    })
}

#[tokio::test]
async fn submit_turn_sync_returns_llm_output() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "hello from child");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "test-child".into(),
                system_prompt: Some("be helpful".into()),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = ops
        .submit_turn(
            parent_id.as_str(),
            SubmitTurnRequest {
                target_session_id: handle.session_id.clone(),
                user_prompt: "say hello".into(),
                wait_for_result: true,
                notify_parent_on_complete: None,
                recycle_on_complete: false,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();

    match result {
        SubmitTurnResult::Completed { content } => {
            assert_eq!(content, "hello from child");
        },
        SubmitTurnResult::Backgrounded { .. } => {
            panic!("expected Completed, got Backgrounded");
        },
    }

    // 父 session 应有 AgentSessionCompleted 事件
    let parent_model = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent_model.agent_sessions.len(), 1);
    assert_eq!(
        parent_model.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );
}

#[tokio::test]
async fn submit_turn_async_returns_backgrounded_and_completes() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "async result");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "async-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = ops
        .submit_turn(
            parent_id.as_str(),
            SubmitTurnRequest {
                target_session_id: handle.session_id.clone(),
                user_prompt: "do async work".into(),
                wait_for_result: false,
                notify_parent_on_complete: Some("[done]".into()),
                recycle_on_complete: false,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();

    match &result {
        SubmitTurnResult::Backgrounded {
            task_id,
            session_id,
        } => {
            assert!(!task_id.is_empty());
            assert_eq!(session_id, &handle.session_id);
        },
        SubmitTurnResult::Completed { .. } => {
            panic!("expected Backgrounded, got Completed");
        },
    }

    // 给后台任务完成
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 触发 process_child_completions 消费已完成的 guard
    // — 这会通过 submit_or_inject 将 notify_parent_on_complete 消息写入父 session
    ops.scheduler.process_child_completions(&parent_id).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 父 session 应有 AgentSessionCompleted
    let parent_model = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent_model.agent_sessions.len(), 1);
    assert_eq!(
        parent_model.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );

    // notify_parent_on_complete 消息应存在
    let has_notify = parent_model.messages.iter().any(|m| {
        m.content.iter().any(|c| matches!(c, astrcode_core::llm::LlmContent::Text { text } if text.contains("[done]")))
    });
    assert!(
        has_notify,
        "notify_parent_on_complete message should be in parent session"
    );
}
