//! 集成测试：ServerSessionOperations 的 submit_turn 同步/异步路径。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::{AgentSessionStatus, EventStore},
    tool::{
        CreateSessionRequest, SessionOperations, SubmitTurnRequest, SubmitTurnResult,
        ToolDefinition,
    },
    types::{SessionId, new_session_id},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_server::test_support::{
    ChildSessionCoordinator, ConfigManager, ServerEventBus, ServerSessionOperations,
    SessionManager, TurnRegistry, TurnScheduler,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::in_memory::InMemoryEventStore;
use astrcode_support::event_fanout::EventFanout;
use tokio::sync::mpsc;

struct StaticTextLlm {
    text: &'static str,
}

/// 在发送 Done 前阻塞，便于在活跃 turn 期间调用 `inject_message`。
struct GateLlm {
    release: Arc<tokio::sync::Notify>,
}

impl GateLlm {
    fn new_pair() -> (Self, Arc<tokio::sync::Notify>) {
        let release = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                release: Arc::clone(&release),
            },
            release,
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for GateLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let release = Arc::clone(&self.release);
        tokio::spawn(async move {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "partial".into(),
            });
            release.notified().await;
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
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

fn build_test_ops_with_llm(
    store: Arc<dyn EventStore>,
    llm_provider: Arc<dyn LlmProvider>,
) -> Arc<ServerSessionOperations> {
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
            thinking_level: None,
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
            thinking_level: None,
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
    let config = Arc::new(ConfigManager::new(
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
    let child_sessions = Arc::new(ChildSessionCoordinator::new(Arc::clone(&session_manager)));
    let scheduler = Arc::new(TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(TurnRegistry::new()),
        Arc::clone(&child_sessions),
    ));
    child_sessions.spawn_completion_watcher(Arc::clone(&scheduler));
    let event_bus = Arc::new(ServerEventBus::new(Arc::new(EventFanout::new(1024))));
    session_manager.bind_event_bus(event_bus);
    Arc::new(ServerSessionOperations {
        session_manager,
        scheduler,
        child_sessions,
    })
}

fn build_test_ops(
    store: Arc<dyn EventStore>,
    llm_text: &'static str,
) -> Arc<ServerSessionOperations> {
    build_test_ops_with_llm(store, Arc::new(StaticTextLlm { text: llm_text }))
}

#[tokio::test]
async fn inject_message_during_active_turn_binds_turn_id() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();

    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "inject-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let _bg = ops
        .submit_turn(
            parent_id.as_str(),
            SubmitTurnRequest {
                target_session_id: handle.session_id.clone(),
                user_prompt: "start turn".into(),
                wait_for_result: false,
                notify_parent_on_complete: None,
                recycle_on_complete: false,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();

    for _ in 0..50 {
        if ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().has_active(&child_id),
        "child turn should be active before inject"
    );

    ops.inject_message(
        parent_id.as_str(),
        child_id.as_str(),
        "mid-turn inject".into(),
    )
    .await
    .unwrap();

    let events = store.replay_events(&child_id).await.unwrap();
    let injected = events
        .iter()
        .find(|e| {
            matches!(
                &e.payload,
                EventPayload::UserMessage { text, .. } if text == "mid-turn inject"
            )
        })
        .expect("injected UserMessage must be durable");
    assert!(
        injected.turn_id.is_some(),
        "active-turn inject must bind turn_id (same as TurnScheduler::inject)"
    );

    release.notify_one();
    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

    // 给后台任务完成；completion watcher 会自动 drain，无需手动调用
    for _ in 0..100 {
        let parent_model = store.session_read_model(&parent_id).await.unwrap();
        if parent_model.agent_sessions.len() == 1
            && parent_model.agent_sessions[0].status == AgentSessionStatus::Completed
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 父 session 应有 AgentSessionCompleted
    let parent_model = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent_model.agent_sessions.len(), 1);
    assert_eq!(
        parent_model.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );

    // notify_parent_on_complete 消息应存在
    let has_notify = parent_model.messages.iter().any(|m| {
        m.message.content.iter().any(|c| {
            matches!(
                c,
                astrcode_core::llm::LlmContent::Text { text } if text.contains("[done]")
            )
        })
    });
    assert!(
        has_notify,
        "notify_parent_on_complete message should be in parent session"
    );
}

#[tokio::test]
async fn submit_turn_async_recycle_on_complete_drains_without_manual_call() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "recycled child output");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "recycle-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let result = ops
        .submit_turn(
            parent_id.as_str(),
            SubmitTurnRequest {
                target_session_id: handle.session_id.clone(),
                user_prompt: "work then recycle".into(),
                wait_for_result: false,
                notify_parent_on_complete: None,
                recycle_on_complete: true,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();

    assert!(
        matches!(result, SubmitTurnResult::Backgrounded { .. }),
        "expected Backgrounded"
    );

    for _ in 0..100 {
        let parent_events = store.replay_events(&parent_id).await.unwrap();
        let completed = parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionCompleted {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        });
        let recycled = parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        });
        if completed && recycled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let parent_events = store.replay_events(&parent_id).await.unwrap();
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionCompleted {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        }),
        "completion watcher should write AgentSessionCompleted"
    );
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        }),
        "completion watcher should recycle child session"
    );
    assert!(
        !ops.scheduler.registry().has_active(&child_id),
        "recycle must release registry without leaving a stale active entry"
    );
}

#[tokio::test]
async fn parent_abort_stops_sync_child_and_recycles() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();

    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "sync-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let ops_for_turn = Arc::clone(&ops);
    let parent_for_turn = parent_id.clone();
    let child_target = handle.session_id.clone();
    let sync_turn = tokio::spawn(async move {
        ops_for_turn
            .submit_turn(
                parent_for_turn.as_str(),
                SubmitTurnRequest {
                    target_session_id: child_target,
                    user_prompt: "sync work".into(),
                    wait_for_result: true,
                    notify_parent_on_complete: None,
                    recycle_on_complete: false,
                    tool_call_id: None,
                },
            )
            .await
    });

    for _ in 0..100 {
        if ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().has_active(&child_id),
        "sync child turn should be active in registry"
    );

    ops.scheduler.abort(&parent_id).await.unwrap();
    release.notify_one();

    let sync_result = sync_turn.await.expect("sync turn task panicked");
    assert!(
        sync_result.is_err(),
        "sync child turn should fail after parent cascade abort"
    );

    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ops.scheduler.registry().has_active(&child_id),
        "child registry entry must be cleared after cascade abort"
    );

    let parent_events = store.replay_events(&parent_id).await.unwrap();
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionFailed {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        }),
        "cascade abort should mark sync child as failed on parent"
    );
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        }),
        "cascade abort should recycle unguarded sync child session"
    );
    assert!(
        store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "recycled child should be removed from parent agent_sessions projection"
    );
}
