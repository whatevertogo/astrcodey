use std::{future, sync::Arc, time::Duration};

use astrcode_context::{compaction::CompactResult, manager::LlmContextAssembler};
use astrcode_core::{
    config::{EffectiveConfig, LlmSettings, OpenAiApiMode},
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    tool::ToolDefinition,
};
use astrcode_protocol::events::ClientNotification;
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

use super::*;
use crate::session::{
    SessionManager, compact_boundary_payload, session_continued_from_compaction_payload,
};

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
            delta: r#"<summary>
1. Primary Request and Intent:
   Compacted conversation summary

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - (none)

4. Errors and fixes:
   - (none)

5. Problem Solving:
   compacted

6. All user messages:
   - (none)

7. Pending Tasks:
   - (none)

8. Current Work:
   compact command

9. Optional Next Step:
   - (none)
</summary>"#
                .into(),
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

struct PendingLlm;

struct InvalidSummaryLlm;

#[async_trait::async_trait]
impl LlmProvider for PendingLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        future::pending().await
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for InvalidSummaryLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "not a compact summary".into(),
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

fn test_runtime_with_settings(
    llm_provider: Arc<dyn LlmProvider>,
    context_settings: astrcode_context::settings::ContextWindowSettings,
) -> Arc<ServerRuntime> {
    Arc::new(ServerRuntime {
        session_manager: Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new()))),
        llm_provider,
        context_assembler: Arc::new(LlmContextAssembler::new(context_settings.clone())),
        auto_compact_failures: Arc::new(crate::agent::AutoCompactFailureTracker::default()),
        extension_runner: Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
            Duration::from_secs(1),
            Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
        )),
        effective: EffectiveConfig {
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
            },
        },
    })
}

fn test_runtime_with_llm(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
    test_runtime_with_settings(
        llm_provider,
        astrcode_context::settings::ContextWindowSettings::default(),
    )
}

fn test_runtime() -> Arc<ServerRuntime> {
    test_runtime_with_llm(Arc::new(MockLlm))
}

async fn recv_event(event_rx: &mut broadcast::Receiver<ClientNotification>) -> ClientNotification {
    tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("event should arrive")
        .expect("event channel should stay open")
}

async fn wait_for_turn_completed(event_rx: &mut broadcast::Receiver<ClientNotification>) -> String {
    loop {
        let notification = recv_event(event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        if let EventPayload::TurnCompleted { finish_reason } = event.payload {
            return finish_reason;
        }
    }
}

async fn drain_until_compact_boundary(
    event_rx: &mut broadcast::Receiver<ClientNotification>,
) -> SessionId {
    loop {
        let notification = recv_event(event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        if let EventPayload::CompactBoundaryCreated {
            continued_session_id,
            ..
        } = event.payload
        {
            return continued_session_id;
        }
    }
}

async fn collect_turn_ids_until_completed(
    event_rx: &mut broadcast::Receiver<ClientNotification>,
) -> (String, Vec<Option<TurnId>>) {
    let mut turn_ids = Vec::new();
    loop {
        let notification = recv_event(event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::TurnStarted
            | EventPayload::UserMessage { .. }
            | EventPayload::AssistantMessageCompleted { .. } => {
                turn_ids.push(event.turn_id);
            },
            EventPayload::TurnCompleted { finish_reason } => {
                turn_ids.push(event.turn_id);
                return (finish_reason, turn_ids);
            },
            _ => {},
        }
    }
}

#[test]
fn compact_payload_helpers_split_projection_and_audit_fields() {
    let compaction = CompactResult {
        pre_tokens: 100,
        post_tokens: 20,
        summary: "summary".into(),
        messages_removed: 2,
        context_messages: vec![LlmMessage::system("hidden context")],
        retained_messages: vec![LlmMessage::user("retained")],
        transcript_path: Some("compact.jsonl".into()),
    };

    let boundary = compact_boundary_payload("manual_command", &compaction, "child".into());
    let continued =
        session_continued_from_compaction_payload("parent".into(), "7".into(), &compaction);

    assert!(matches!(
        boundary,
        EventPayload::CompactBoundaryCreated {
            continued_session_id,
            transcript_path: Some(path),
            ..
        } if continued_session_id == "child" && path == "compact.jsonl"
    ));
    assert!(matches!(
        continued,
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id,
            parent_cursor,
            context_messages,
            retained_messages,
            ..
        } if parent_session_id == "parent"
            && parent_cursor == "7"
            && context_messages.len() == 1
            && retained_messages.len() == 1
    ));
}

#[tokio::test]
async fn record_and_broadcast_updates_projection_before_broadcast() {
    let runtime = test_runtime();
    let start_event = runtime
        .session_manager
        .create(".", "mock-model", 2048, None)
        .await
        .unwrap();
    let sid = start_event.session_id.clone();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);

    record_and_broadcast(
        &runtime,
        &event_tx,
        &sid,
        None,
        EventPayload::SystemPromptConfigured {
            text: "ordered prompt".into(),
            fingerprint: "fingerprint".into(),
        },
    )
    .await
    .unwrap();

    let ClientNotification::Event(event) = recv_event(&mut event_rx).await else {
        panic!("expected event notification");
    };
    assert!(event.seq.is_some());

    let model = runtime.session_manager.read_model(&sid).await.unwrap();
    assert_eq!(model.system_prompt.as_deref(), Some("ordered prompt"));
}

#[tokio::test]
async fn create_session_configures_system_prompt() {
    let runtime = test_runtime();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();

    let mut saw_configured = false;
    for _ in 0..2 {
        if let ClientNotification::Event(event) = recv_event(&mut event_rx).await {
            if let EventPayload::SystemPromptConfigured { text, fingerprint } = event.payload {
                saw_configured = true;
                assert!(text.contains("# Identity"));
                assert!(!fingerprint.is_empty());
            }
        }
    }
    assert!(saw_configured);

    let state = runtime.session_manager.read_model(&sid).await.unwrap();
    assert!(
        state
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("# Identity"))
    );
    assert!(state.messages.is_empty());
}

#[tokio::test]
async fn submit_prompt_reuses_session_system_prompt() {
    let runtime = test_runtime();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    let initial_prompt = {
        let state = runtime.session_manager.read_model(&sid).await.unwrap();
        state.system_prompt.clone()
    };

    handler
        .submit_prompt_for_session(sid.clone(), "one".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    handler
        .submit_prompt_for_session(sid.clone(), "two".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime.session_manager.read_model(&sid).await.unwrap();
    assert_eq!(state.system_prompt, initial_prompt);
}

#[tokio::test]
async fn submit_prompt_configures_missing_session_system_prompt() {
    let runtime = test_runtime();
    let start_event = runtime
        .session_manager
        .create(".", "mock-model", 2048, None)
        .await
        .unwrap();
    let sid = start_event.session_id.clone();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    handler
        .submit_prompt_for_session(sid.clone(), "hello".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime.session_manager.read_model(&sid).await.unwrap();
    assert!(
        state
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("# Identity"))
    );
}

#[tokio::test]
async fn submit_prompt_uses_one_turn_id_for_turn_events() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(test_runtime(), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_prompt_for_session(sid, "hi".into())
        .await
        .unwrap();
    let (finish_reason, turn_ids) = collect_turn_ids_until_completed(&mut event_rx).await;
    assert_eq!(finish_reason, "stop");

    assert!(
        turn_ids.len() >= 4,
        "expected turn lifecycle, user and assistant events"
    );
    let first = turn_ids[0].clone();
    assert!(first.is_some(), "turn events should carry a turn_id");
    assert!(
        turn_ids.iter().all(|turn_id| *turn_id == first),
        "all events in one prompt should share the same turn_id"
    );
}

#[tokio::test]
async fn submit_prompt_rejects_second_running_turn() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler =
        CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_prompt_for_session(sid.clone(), "first".into())
        .await
        .unwrap();
    let error = handler
        .submit_prompt_for_session(sid.clone(), "second".into())
        .await
        .unwrap_err();
    assert!(error.contains("already running"));

    let mut saw_busy = false;
    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Error { code: 40900, .. } = notification {
            saw_busy = true;
            break;
        }
    }
    assert!(saw_busy, "second prompt should be rejected while turn runs");

    handler.abort_session(sid).await.unwrap();
}

#[tokio::test]
async fn abort_stops_active_turn_and_records_completion() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler =
        CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_prompt_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();

    handler.abort_session(sid).await.unwrap();

    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
}

#[tokio::test]
async fn compact_session_rejects_running_turn_without_compaction_started() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
    let handler =
        CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_prompt_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();
    while event_rx.try_recv().is_ok() {}

    let error = handler.compact_session(sid.clone()).await.unwrap_err();
    assert_eq!(error, "Cannot compact while a turn is running");

    let mut saw_conflict = false;
    while let Ok(notification) = event_rx.try_recv() {
        match notification {
            ClientNotification::Error { code, .. } => {
                saw_conflict |= code == 40900;
            },
            ClientNotification::Event(event) => {
                assert!(
                    !matches!(event.payload, EventPayload::CompactionStarted),
                    "rejected compact must not leave clients in compacting state"
                );
            },
            _ => {},
        }
    }
    assert!(saw_conflict);

    handler.abort_session(sid).await.unwrap();
}

#[tokio::test]
async fn stale_agent_finish_after_abort_is_ignored() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler =
        CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    let turn_id = handler
        .submit_prompt_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();
    handler.abort_session(sid.clone()).await.unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");

    handler
        .tx
        .send(CommandMessage::AgentTurnFinished {
            session_id: sid,
            turn_id,
            output: AgentTurnOutput {
                text: "late".into(),
                finish_reason: "stop".into(),
                tool_results: vec![],
                auto_compaction: None,
            },
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Event(event) = notification {
            if matches!(event.payload, EventPayload::TurnCompleted { .. }) {
                panic!("stale AgentTurnFinished should not emit a second completion");
            }
        }
    }
}

#[tokio::test]
async fn compact_command_rewrites_provider_history_without_exposing_summary() {
    let settings = astrcode_context::settings::ContextWindowSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let parent_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three"] {
        handler
            .submit_prompt_for_session(parent_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let child_id = handler
        .compact_session(parent_id.clone())
        .await
        .unwrap()
        .unwrap();
    let continued_session_id = drain_until_compact_boundary(&mut event_rx).await;
    assert_eq!(child_id, continued_session_id);

    let parent_state = runtime
        .session_manager
        .read_model(&parent_id)
        .await
        .unwrap();
    assert!(parent_state.context_messages.is_empty());
    assert!(!parent_state.messages.is_empty());

    let state = runtime.session_manager.read_model(&child_id).await.unwrap();
    assert_eq!(state.parent_session_id.as_deref(), Some(parent_id.as_str()));
    assert!(!state.context_messages.is_empty());
    assert!(state.provider_messages().iter().any(|message| {
        message_to_dto(message)
            .content
            .contains("<compact_summary>")
    }));
    assert!(state.messages.iter().all(|message| {
        !message_to_dto(message)
            .content
            .contains("<compact_summary>")
    }));
}

#[tokio::test]
async fn compact_command_compacts_existing_hidden_context_again() {
    let settings = astrcode_context::settings::ContextWindowSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let first_session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three", "four"] {
        handler
            .submit_prompt_for_session(first_session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let first_child_id = handler
        .compact_session(first_session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first_child_id,
        drain_until_compact_boundary(&mut event_rx).await
    );
    let first_summary = {
        let state = runtime
            .session_manager
            .read_model(&first_child_id)
            .await
            .unwrap();
        message_to_dto(&state.context_messages[0]).content
    };

    handler
        .submit_prompt_for_session(first_child_id.clone(), "five".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    let second_child_id = handler
        .compact_session(first_child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        second_child_id,
        drain_until_compact_boundary(&mut event_rx).await
    );

    let state = runtime
        .session_manager
        .read_model(&second_child_id)
        .await
        .unwrap();
    let second_summary = message_to_dto(&state.context_messages[0]).content;
    assert!(
        second_summary.contains("Compacted conversation summary"),
        "second compact should preserve a provider summary"
    );
    assert!(
        first_summary.contains("Compacted conversation summary"),
        "first compact should preserve a provider summary"
    );
}

#[tokio::test]
async fn auto_compact_switches_active_session_to_continuation_child() {
    let settings = astrcode_context::settings::ContextWindowSettings {
        compact_threshold_percent: 0.0,
        ..Default::default()
    };
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let parent_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .session_manager
            .append_event(Event::new(
                parent_id.clone(),
                None,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(20)),
                },
            ))
            .await
            .unwrap();
        runtime
            .session_manager
            .append_event(Event::new(
                parent_id.clone(),
                None,
                EventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("old answer {index} {}", "y ".repeat(20)),
                },
            ))
            .await
            .unwrap();
    }

    handler
        .submit_prompt_for_session(parent_id.clone(), "current".into())
        .await
        .unwrap();
    let mut compaction_started_count = 0;
    let mut child_id = None;
    let mut turn_completed_session = None;
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should remain open");
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::CompactionStarted => {
                compaction_started_count += 1;
                assert_eq!(event.session_id, parent_id);
            },
            EventPayload::CompactBoundaryCreated {
                continued_session_id,
                ..
            } => {
                assert!(
                    turn_completed_session.is_none(),
                    "compact boundary should be created before turn completion"
                );
                assert_eq!(event.session_id, parent_id);
                child_id = Some(continued_session_id);
            },
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "stop");
                turn_completed_session = Some(event.session_id);
                if child_id.is_some() {
                    break;
                }
            },
            _ => {},
        }
    }
    assert_eq!(compaction_started_count, 1);
    let child_id = child_id.expect("compact boundary should create a child session");
    assert_eq!(turn_completed_session.as_deref(), Some(child_id.as_str()));

    let parent = runtime
        .session_manager
        .read_model(&parent_id)
        .await
        .unwrap();
    assert!(parent.context_messages.is_empty());
    let child = runtime.session_manager.read_model(&child_id).await.unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some(parent_id.as_str()));
    assert!(!child.context_messages.is_empty());
    assert!(child.messages.iter().any(|message| {
        message_to_dto(message)
            .content
            .contains("Compacted conversation summary")
    }));
}

#[tokio::test]
async fn compact_command_does_not_fallback_when_summary_is_invalid() {
    let runtime = test_runtime_with_llm(Arc::new(InvalidSummaryLlm));
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three"] {
        handler
            .submit_prompt_for_session(sid.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }
    while event_rx.try_recv().is_ok() {}

    assert!(
        handler
            .compact_session(sid.clone())
            .await
            .unwrap()
            .is_none()
    );

    let state = runtime.session_manager.read_model(&sid).await.unwrap();
    assert!(state.context_messages.is_empty());

    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Event(event) = notification {
            assert!(
                !matches!(event.payload, EventPayload::CompactionStarted),
                "failed compact must not leave clients in compacting state"
            );
        }
    }
}
