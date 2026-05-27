//! SSOT Route A 行为回归：turn 历史仅来自 EventStore projection。

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        OpenAiApiMode,
    },
    event::EventPayload,
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    storage::EventStore,
    tool::ToolDefinition,
    types::{new_message_id, new_session_id, new_turn_id},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{Session, SessionCreateParams, SessionRuntimeServices, SessionRuntimeState};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

fn test_caps(llm: Arc<dyn LlmProvider>) -> Arc<SessionRuntimeServices> {
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
            context_limit: 200_000,
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
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 200_000,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        },
        context: ContextSettings::default(),
        agent: AgentSettings::default(),
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

async fn spawn_session(llm: Arc<dyn LlmProvider>) -> Session {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps(llm);
    let sid = new_session_id();
    let runtime = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "mock-model".into(),
    ));
    let working_dir = std::env::temp_dir().join(sid.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    let session = Session::create_with_params(SessionCreateParams {
        store,
        sid: sid.clone(),
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent: None,
        tool_policy: None,
        source_extension: None,
        runtime,
        caps,
    })
    .await
    .unwrap();
    session.refresh_tools(&working_dir.to_string_lossy()).await;
    session
}

struct ToolLoopLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ToolLoopLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        if round == 0 {
            let _ = tx.send(LlmEvent::ToolCallStart {
                call_id: "call-a".into(),
                name: "unknown_tool".into(),
                arguments: "{}".into(),
            });
            let _ = tx.send(LlmEvent::ToolCallStart {
                call_id: "call-b".into(),
                name: "unknown_tool".into(),
                arguments: "{}".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "tool_calls".into(),
            });
        } else {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "final answer".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn ssot_tool_loop_projection_matches_provider_messages() {
    let llm = Arc::new(ToolLoopLlm {
        calls: AtomicUsize::new(0),
    });
    let session = spawn_session(llm).await;
    let turn_id = new_turn_id();
    let handle = session.submit("run tools".into(), turn_id).await.unwrap();
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let model = session.read_model().await.unwrap();
    let messages = model.provider_messages();
    let user_count = messages.iter().filter(|m| m.role == LlmRole::User).count();
    assert!(user_count >= 1);
    let assistant_with_tools = messages.iter().any(|message| {
        message.role == LlmRole::Assistant
            && message
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. }))
    });
    assert!(assistant_with_tools, "expected merged assistant+tool_calls");
    let tool_results = messages.iter().filter(|m| m.role == LlmRole::Tool).count();
    assert_eq!(tool_results, 2, "expected two tool result messages");
    assert!(
        messages.iter().any(|m| m.role == LlmRole::Assistant
            && m.joined_display_text("\n").contains("final answer")),
        "expected final assistant text in projection"
    );
}

struct ThinkingToolsLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ThinkingToolsLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        if round == 0 {
            let _ = tx.send(LlmEvent::ThinkingDelta {
                delta: "private reasoning".into(),
            });
            let _ = tx.send(LlmEvent::ToolCallStart {
                call_id: "call-1".into(),
                name: "unknown_tool".into(),
                arguments: "{}".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "tool_calls".into(),
            });
        } else {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "done".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn ssot_thinking_and_tools_merge_in_projection() {
    let session = spawn_session(Arc::new(ThinkingToolsLlm {
        calls: AtomicUsize::new(0),
    }))
    .await;
    let turn_id = new_turn_id();
    let handle = session
        .submit("think then tool".into(), turn_id)
        .await
        .unwrap();
    let _ = handle.wait().await.unwrap();

    let messages = session.read_model().await.unwrap().provider_messages();
    let merged = messages.iter().find(|message| {
        message.role == LlmRole::Assistant
            && message.reasoning_content.as_deref() == Some("private reasoning")
            && message
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. }))
    });
    assert!(
        merged.is_some(),
        "expected reasoning_content merged with tool_calls on one assistant message"
    );
}

struct DelayThenCompleteLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for DelayThenCompleteLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        if round == 0 {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "call-delay".into(),
                    name: "unknown_tool".into(),
                    arguments: "{}".into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "tool_calls".into(),
                });
            });
        } else {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "step two".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn ssot_tool_only_turn_emits_assistant_shell_before_tool_requests() {
    let session = spawn_session(Arc::new(DelayThenCompleteLlm {
        calls: AtomicUsize::new(0),
    }))
    .await;
    let turn_id = new_turn_id();
    let handle = session.submit("tool only".into(), turn_id).await.unwrap();
    let _ = handle.wait().await.unwrap();

    let messages = session.read_model().await.unwrap().messages;
    assert!(
        messages.iter().any(|message| {
            message.message.role == LlmRole::Assistant
                && message.message.content.iter().any(|content| {
                    matches!(
                        content,
                        LlmContent::ToolCall { call_id, .. } if call_id == "call-delay"
                    )
                })
        }),
        "tool-only turn must durable assistant shell then ToolCallRequested so projection merges \
         tool_calls"
    );
}

#[tokio::test]
async fn ssot_mid_turn_inject_visible_on_next_prepare() {
    let llm = Arc::new(DelayThenCompleteLlm {
        calls: AtomicUsize::new(0),
    });
    let session = Arc::new(spawn_session(llm).await);
    let turn_id = new_turn_id();
    let session_for_inject = Arc::clone(&session);
    let inject_turn = turn_id.clone();
    let inject = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = session_for_inject
            .emit_durable(
                Some(&inject_turn),
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "mid-turn inject".into(),
                },
            )
            .await;
    });

    let handle = session.submit("start".into(), turn_id).await.unwrap();
    let _ = handle.wait().await.unwrap();
    inject.await.unwrap();

    let model = session.read_model().await.unwrap();
    assert!(
        model.messages.iter().any(|message| {
            message.message.role == LlmRole::User
                && message.message.content.iter().any(|content| {
                    matches!(
                        content,
                        LlmContent::Text { text } if text == "mid-turn inject"
                    )
                })
        }),
        "injected user message must appear in read_model without TurnState drain"
    );
}
