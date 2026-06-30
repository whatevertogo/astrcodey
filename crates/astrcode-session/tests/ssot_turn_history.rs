//! SSOT Route A 行为回归：turn 历史仅来自 EventStore projection。

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    event::EventPayload,
    llm::{
        LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, LlmTokenUsage,
        LlmTokenUsageSource, ModelLimits, ProviderInputTokenCount,
    },
    storage::EventStore,
    tool::ToolDefinition,
    types::{new_message_id, new_session_id, new_turn_id},
};
use astrcode_session::{Session, SessionCreateParams, SessionRuntimeServices, SessionRuntimeState};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

mod common;

fn test_caps(llm: Arc<dyn LlmProvider>) -> Arc<SessionRuntimeServices> {
    common::test_runtime_services(llm)
}

async fn spawn_session(llm: Arc<dyn LlmProvider>) -> Session {
    let (session, _, _) = spawn_session_with_store(llm).await;
    session
}

async fn spawn_session_with_store(
    llm: Arc<dyn LlmProvider>,
) -> (
    Session,
    Arc<dyn EventStore>,
    astrcode_core::types::SessionId,
) {
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
        store: Arc::clone(&store),
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
    (session, store, sid)
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
    let handle = session
        .submit("run tools".into(), vec![], turn_id)
        .await
        .unwrap();
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

struct UsageLlm;

struct FailingGenerateLlm;

#[async_trait::async_trait]
impl LlmProvider for FailingGenerateLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        Err(LlmError::ClientError {
            status: 429,
            message: "Too Many Requests".into(),
        })
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn provider_start_error_is_persisted_as_durable_error() {
    let (session, store, sid) = spawn_session_with_store(Arc::new(FailingGenerateLlm)).await;
    let turn_id = new_turn_id();
    let handle = session
        .submit("trigger provider error".into(), vec![], turn_id)
        .await
        .unwrap();

    let result = handle.wait().await.unwrap();
    assert!(result.output.is_err(), "{:?}", result.output);

    let events = store.replay_events(&sid).await.unwrap();
    let errors = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ErrorOccurred { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "expected one durable ErrorOccurred");
    assert!(errors[0].contains("Too Many Requests"));
}

#[async_trait::async_trait]
impl LlmProvider for UsageLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::Usage {
            usage: LlmTokenUsage {
                input_tokens: Some(100),
                cached_input_tokens: Some(64),
                cache_creation_input_tokens: None,
                output_tokens: Some(20),
                reasoning_output_tokens: Some(5),
                total_tokens: Some(120),
                source: None,
            },
        });
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 12345,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn token_usage_is_persisted_as_durable_event() {
    let (session, store, sid) = spawn_session_with_store(Arc::new(UsageLlm)).await;
    let turn_id = new_turn_id();
    let handle = session
        .submit("record usage".into(), vec![], turn_id)
        .await
        .unwrap();
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let events = store.replay_events(&sid).await.unwrap();
    let assistant_completed_index = events
        .iter()
        .position(|event| {
            matches!(
                event.payload,
                EventPayload::AssistantMessageCompleted { .. }
            )
        })
        .expect("expected AssistantMessageCompleted event");
    let token_usage_index = events
        .iter()
        .position(|event| matches!(event.payload, EventPayload::TokenUsageRecorded { .. }))
        .expect("expected TokenUsageRecorded event");
    assert!(
        token_usage_index > assistant_completed_index,
        "TokenUsageRecorded should be written after AssistantMessageCompleted"
    );

    let token_usage = match &events[token_usage_index].payload {
        EventPayload::TokenUsageRecorded {
            usage,
            model_context_window,
        } => Some((usage, model_context_window)),
        _ => None,
    };

    let Some((usage, model_context_window)) = token_usage else {
        panic!("expected TokenUsageRecorded event");
    };
    assert_eq!(*model_context_window, 12345);
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.cached_input_tokens, Some(64));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.reasoning_output_tokens, Some(5));
    assert_eq!(usage.total_tokens, Some(120));
}

struct NoUsageCountingLlm;

#[async_trait::async_trait]
impl LlmProvider for NoUsageCountingLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    async fn count_input_tokens(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        Ok(ProviderInputTokenCount::provider_count(321))
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 12345,
            max_output_tokens: 4096,
        }
    }
}

#[tokio::test]
async fn token_usage_missing_stream_usage_records_provider_count_fallback() {
    let (session, store, sid) = spawn_session_with_store(Arc::new(NoUsageCountingLlm)).await;
    let turn_id = new_turn_id();
    let handle = session
        .submit("record fallback usage".into(), vec![], turn_id)
        .await
        .unwrap();
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let events = store.replay_events(&sid).await.unwrap();
    let usage = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::TokenUsageRecorded { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("expected fallback TokenUsageRecorded event");

    assert_eq!(usage.input_tokens, Some(321));
    assert_eq!(usage.cached_input_tokens, None);
    assert_eq!(usage.cache_creation_input_tokens, None);
    assert_eq!(usage.output_tokens, None);
    assert_eq!(
        usage.source,
        Some(LlmTokenUsageSource::ProviderCountFallback)
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
        .submit("think then tool".into(), vec![], turn_id)
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
    let handle = session
        .submit("tool only".into(), vec![], turn_id)
        .await
        .unwrap();
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
                    attachments: vec![],
                },
            )
            .await;
    });

    let handle = session
        .submit("start".into(), vec![], turn_id)
        .await
        .unwrap();
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
