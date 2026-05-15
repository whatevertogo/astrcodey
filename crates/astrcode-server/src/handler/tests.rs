use std::{
    fs, future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use astrcode_context::{compaction::CompactResult, context_engine::LlmContextAssembler};
use astrcode_core::{
    config::{ContextSettings, EffectiveConfig, LlmSettings, OpenAiApiMode},
    event::{Event, EventPayload, Phase},
    extension::{
        CommandContext, Extension, ExtensionCommandResult, ExtensionError, ExtensionEvent,
        HookMode, HookResult, LifecycleContext, Registrar, SlashCommand,
    },
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    storage::EventStore,
    tool::ToolDefinition,
    types::{SessionId, ToolCallId},
};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

use astrcode_session::{Session, compact_boundary_payload, session_continued_from_compaction_payload};

use super::*;
use crate::agent::AgentTurnOutput;

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

struct FailSessionStartExtension;

#[derive(Clone, Default)]
struct CapturingLlm {
    messages: Arc<Mutex<Vec<LlmMessage>>>,
}

struct StaticCommandExtension {
    id: &'static str,
    command_name: &'static str,
}

#[async_trait::async_trait]
impl Extension for StaticCommandExtension {
    fn id(&self) -> &str {
        self.id
    }

    fn register(&self, reg: &mut Registrar) {
        let command_name = self.command_name;
        reg.command(
            SlashCommand {
                name: command_name.into(),
                description: "Static test command".into(),
                args_schema: None,
            },
            Arc::new(StaticCommandHandler {
                command_name: command_name.to_string(),
            }),
        );
    }
}

struct StaticCommandHandler {
    command_name: String,
}

#[async_trait::async_trait]
impl astrcode_core::extension::CommandHandler for StaticCommandHandler {
    async fn execute(
        &self,
        command_name: &str,
        _args: &str,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        if command_name == self.command_name {
            return Ok(ExtensionCommandResult::display("plugin command", false));
        }
        Err(ExtensionError::NotFound(command_name.into()))
    }
}

#[async_trait::async_trait]
impl Extension for FailSessionStartExtension {
    fn id(&self) -> &str {
        "fail-session-start"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_event(
            ExtensionEvent::SessionStart,
            HookMode::Blocking,
            0,
            Arc::new(FailSessionStartHandler),
        );
    }
}

struct FailSessionStartHandler;

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for FailSessionStartHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        Err(ExtensionError::Internal("session start failed".into()))
    }
}

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

#[async_trait::async_trait]
impl LlmProvider for CapturingLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        *self.messages.lock().unwrap() = messages;
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "captured".into(),
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
    context_settings: astrcode_context::ContextSettings,
) -> Arc<ServerRuntime> {
    Arc::new(ServerRuntime {
        event_store: Arc::new(InMemoryEventStore::new()) as Arc<dyn EventStore>,
        llm_provider: Arc::new(parking_lot::RwLock::new(llm_provider)),
        context_assembler: Arc::new(LlmContextAssembler::new(context_settings.clone())),
        auto_compact_failures: Arc::new(crate::agent::AutoCompactFailureTracker::default()),
        background_tasks: Default::default(),
        extension_runner: Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
            Duration::from_secs(1),
        )),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        config_store: Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/test-config.json"),
        )),
        raw_config: parking_lot::RwLock::new(astrcode_core::config::Config::default()),
        effective: parking_lot::RwLock::new(EffectiveConfig {
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
            },
            context: ContextSettings {
                auto_compact_enabled: context_settings.auto_compact_enabled,
                compact_threshold_percent: context_settings.compact_threshold_percent,
                compact_max_retry_attempts: context_settings.compact_max_retry_attempts,
                compact_max_output_tokens: context_settings.compact_max_output_tokens,
                post_compact_max_files: context_settings.post_compact_max_files,
                post_compact_token_budget: context_settings.post_compact_token_budget,
                post_compact_max_tokens_per_file: context_settings.post_compact_max_tokens_per_file,
            },
        }),
        agent_session_control: crate::bootstrap::AgentSessionControlSlot::new(
            parking_lot::RwLock::new(None),
        ),
    })
}

fn test_runtime_with_llm(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
    test_runtime_with_settings(llm_provider, astrcode_context::ContextSettings::default())
}

fn test_runtime() -> Arc<ServerRuntime> {
    test_runtime_with_llm(Arc::new(MockLlm))
}

fn unique_workspace(name: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("astrcode-{name}-{timestamp}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_project_skill(workspace: &Path, id: &str, content: &str) {
    let skill_dir = workspace.join(".astrcode").join("skills").join(id);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

fn compacted_session_id(outcome: ManualCompactOutcome) -> SessionId {
    match outcome {
        ManualCompactOutcome::Compacted { session_id } => session_id,
        ManualCompactOutcome::Skipped { message } => {
            panic!("expected compact, compact was skipped: {message}")
        },
    }
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
        } if continued_session_id.as_str() == "child" && path == "compact.jsonl"
    ));
    assert!(matches!(
        continued,
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id,
            parent_cursor,
            context_messages,
            retained_messages,
            ..
        } if parent_session_id.as_str() == "parent"
            && parent_cursor == "7"
            && context_messages.len() == 1
            && retained_messages.len() == 1
    ));
}

#[tokio::test]
async fn record_and_broadcast_updates_projection_before_broadcast() {
    let runtime = test_runtime();
    let session = Session::create(runtime.event_store.clone(), ".", "mock-model", None)
        .await
        .unwrap();
    let sid = session.id().clone();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);

    record_and_broadcast(
        &runtime.event_store,
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

    let model = runtime.event_store.session_read_model(&sid).await.unwrap();
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
                assert!(text.contains("[Identity]"));
                assert!(!fingerprint.is_empty());
            }
        }
    }
    assert!(saw_configured);

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert!(
        state
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("[Identity]"))
    );
    assert!(state.messages.is_empty());
}

#[tokio::test]
async fn client_create_session_reports_start_hook_failure() {
    let runtime = test_runtime();
    runtime
        .extension_runner
        .register(Arc::new(FailSessionStartExtension))
        .await;
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(runtime, event_tx);

    let error = handler
        .handle(ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("session start failed"));
    let mut saw_error = false;
    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Error { code, message } = notification {
            saw_error = code == -32603 && message.contains("session start failed");
            break;
        }
    }
    assert!(saw_error, "client should receive create-session failure");
}

#[tokio::test]
async fn submit_prompt_reuses_session_system_prompt() {
    let runtime = test_runtime();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    let initial_prompt = {
        let state = runtime.event_store.session_read_model(&sid).await.unwrap();
        state.system_prompt.clone()
    };

    handler
        .submit_input_for_session(sid.clone(), "one".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    handler
        .submit_input_for_session(sid.clone(), "two".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert_eq!(state.system_prompt, initial_prompt);
}

#[tokio::test]
async fn submit_prompt_configures_missing_session_system_prompt() {
    let runtime = test_runtime();
    let session = Session::create(runtime.event_store.clone(), ".", "mock-model", None)
        .await
        .unwrap();
    let sid = session.id().clone();
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    handler
        .submit_input_for_session(sid.clone(), "hello".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert!(
        state
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("[Identity]"))
    );
}

#[tokio::test]
async fn submit_prompt_uses_one_turn_id_for_turn_events() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(test_runtime(), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid, "hi".into())
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
async fn stale_pending_tool_calls_are_repaired_on_explicit_repair() {
    let runtime = test_runtime();
    let session = Session::create(runtime.event_store.clone(), ".", "mock", None)
        .await
        .unwrap();
    let sid = session.id().clone();
    runtime
        .event_store
        .append_event(Event::new(
            sid.clone(),
            Some("stale-turn".into()),
            EventPayload::ToolCallRequested {
                call_id: "call-1".into(),
                tool_name: "todoWrite".into(),
                arguments: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let stale_state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert_eq!(stale_state.phase, Phase::CallingTool);
    assert!(
        stale_state
            .pending_tool_calls
            .contains(&ToolCallId::from("call-1"))
    );

    let (event_tx, _) = broadcast::channel(16);
    let (actor_tx, _actor_rx) = mpsc::unbounded_channel();
    let handler = CommandHandler::new(Arc::clone(&runtime), event_tx, actor_tx);

    handler.repair_stale_pending_tool_calls(&sid).await.unwrap();

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert_eq!(state.phase, Phase::Idle);
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.messages.iter().any(|message| {
        message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    is_error
                } if tool_call_id == "call-1"
                    && *is_error
                    && content.contains("interrupted before completion")
            )
        })
    }));
}

#[tokio::test]
async fn submit_prompt_rejects_second_running_turn() {
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
    let handler =
        CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "first".into())
        .await
        .unwrap();
    let error = handler
        .submit_input_for_session(sid.clone(), "second".into())
        .await
        .unwrap_err();
    assert!(
        matches!(error, HandlerError::TurnAlreadyRunning),
        "expected TurnAlreadyRunning, got {error:?}"
    );

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
        .submit_input_for_session(sid.clone(), "keep running".into())
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
        .submit_input_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();
    while event_rx.try_recv().is_ok() {}

    let error = handler.compact_session(sid.clone()).await.unwrap_err();
    assert!(
        matches!(error, HandlerError::CompactBlocked),
        "expected CompactBlocked, got {error:?}"
    );

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
    let PromptSubmission::Accepted { turn_id } = handler
        .submit_input_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap()
    else {
        panic!("expected Accepted");
    };
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
    let settings = astrcode_context::ContextSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three"] {
        handler
            .submit_input_for_session(session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let compacted_id = handler
        .compact_session(session_id.clone())
        .await
        .map(compacted_session_id)
        .unwrap();
    assert_eq!(
        compacted_id, session_id,
        "same-session compact keeps session_id"
    );
    let continued_session_id = drain_until_compact_boundary(&mut event_rx).await;
    assert_eq!(continued_session_id, session_id);

    let state = runtime
        .event_store
        .session_read_model(&session_id)
        .await
        .unwrap();
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
async fn slash_compact_uses_backend_command_without_user_message() {
    let runtime = test_runtime_with_settings(
        Arc::new(MockLlm),
        astrcode_context::ContextSettings::default(),
    );
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three"] {
        handler
            .submit_input_for_session(session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let result = handler
        .submit_input_for_session(session_id.clone(), "/compact".into())
        .await
        .unwrap();
    assert!(matches!(result, PromptSubmission::Handled { .. }));
    let continued_session_id = drain_until_compact_boundary(&mut event_rx).await;
    assert_eq!(continued_session_id, session_id, "same-session compact");

    let state = runtime
        .event_store
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(
        state
            .messages
            .iter()
            .all(|message| message_to_dto(message).content != "/compact")
    );
}

#[tokio::test]
async fn unknown_slash_command_does_not_enter_llm_or_transcript() {
    let runtime = test_runtime();
    let (event_tx, _) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let error = handler
        .submit_input_for_session(sid.clone(), "/missing-command".into())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, HandlerError::UnknownCommand(cmd) if cmd == "missing-command"),
        "expected UnknownCommand(missing-command), got {error:?}"
    );
    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert!(state.messages.is_empty());
}

#[tokio::test]
async fn skill_slash_command_injects_transient_instructions_only() {
    let workspace = unique_workspace("skill-slash-command");
    write_project_skill(
        &workspace,
        "reviewnow",
        "---\ndescription: Review code.\n---\nUse this skill to review code.",
    );
    let llm = CapturingLlm::default();
    let captured_messages = Arc::clone(&llm.messages);
    let runtime = test_runtime_with_llm(Arc::new(llm));
    runtime
        .extension_runner
        .register(astrcode_extension_skill::extension())
        .await;
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);
    let sid = handler
        .create_session(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();

    let result = handler
        .submit_input_for_session(sid.clone(), "/reviewnow src/lib.rs".into())
        .await
        .unwrap();
    assert!(matches!(result, PromptSubmission::Accepted { .. }));
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let captured = captured_messages.lock().unwrap().clone();
    let system_text = captured
        .iter()
        .filter(|message| message.role == LlmRole::System)
        .map(|message| message_to_dto(message).content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system_text.contains("[Slash Command Instructions]"));
    assert!(system_text.contains("<skill-name>reviewnow</skill-name>"));
    assert!(system_text.contains("Invocation arguments: src/lib.rs"));

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
    assert!(
        state
            .messages
            .iter()
            .any(|message| message_to_dto(message).content == "/reviewnow src/lib.rs")
    );
    assert!(
        state
            .messages
            .iter()
            .all(|message| !message_to_dto(message).content.contains("<skill-name>"))
    );
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn command_list_keeps_reserved_and_plugin_priority_over_skills() {
    let workspace = unique_workspace("slash-command-priority");
    write_project_skill(
        &workspace,
        "compact",
        "---\ndescription: Skill named compact.\n---\nShould never override builtin.",
    );
    write_project_skill(
        &workspace,
        "reviewnow",
        "---\ndescription: Skill named reviewnow.\n---\nShould not override plugin.",
    );
    let runtime = test_runtime();
    runtime
        .extension_runner
        .register(astrcode_extension_skill::extension())
        .await;
    runtime
        .extension_runner
        .register(Arc::new(StaticCommandExtension {
            id: "test-plugin",
            command_name: "reviewnow",
        }))
        .await;
    let (event_tx, _) = tokio::sync::broadcast::channel(64);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);
    let sid = handler
        .create_session(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();

    let commands = handler.command_infos_for_session(sid).await.unwrap();

    let compact_commands = commands
        .iter()
        .filter(|command| command.name == "compact")
        .collect::<Vec<_>>();
    assert_eq!(compact_commands.len(), 1);
    assert_eq!(compact_commands[0].source, "builtin");
    let reviewnow = commands
        .iter()
        .find(|command| command.name == "reviewnow")
        .expect("reviewnow command");
    assert_eq!(reviewnow.source, "plugin");
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn compact_command_compacts_existing_hidden_context_again() {
    let settings = astrcode_context::ContextSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three", "four"] {
        handler
            .submit_input_for_session(session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let first_compacted = handler
        .compact_session(session_id.clone())
        .await
        .map(compacted_session_id)
        .unwrap();
    assert_eq!(first_compacted, session_id, "same-session compact");
    assert_eq!(
        session_id,
        drain_until_compact_boundary(&mut event_rx).await
    );
    let first_summary = {
        let state = runtime
            .event_store
            .session_read_model(&session_id)
            .await
            .unwrap();
        message_to_dto(&state.context_messages[0]).content
    };

    handler
        .submit_input_for_session(session_id.clone(), "five".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    let second_compacted = handler
        .compact_session(session_id.clone())
        .await
        .map(compacted_session_id)
        .unwrap();
    assert_eq!(second_compacted, session_id, "same-session compact again");
    assert_eq!(
        session_id,
        drain_until_compact_boundary(&mut event_rx).await
    );

    let state = runtime
        .event_store
        .session_read_model(&session_id)
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
async fn auto_compact_applies_same_session_boundary() {
    let settings = astrcode_context::ContextSettings {
        compact_threshold_percent: 0.0,
        ..Default::default()
    };
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .event_store
            .append_event(Event::new(
                session_id.clone(),
                None,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(20)),
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store
            .append_event(Event::new(
                session_id.clone(),
                None,
                EventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("old answer {index} {}", "y ".repeat(20)),
                    reasoning_content: None,
                },
            ))
            .await
            .unwrap();
    }

    handler
        .submit_input_for_session(session_id.clone(), "current".into())
        .await
        .unwrap();
    let mut compaction_started_count = 0;
    let mut compact_boundary_seen = false;
    let mut turn_completed = false;
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
                assert_eq!(event.session_id, session_id);
            },
            EventPayload::CompactBoundaryCreated {
                continued_session_id,
                ..
            } => {
                assert!(
                    !turn_completed,
                    "compact boundary should be created before turn completion"
                );
                assert_eq!(event.session_id, session_id);
                assert_eq!(continued_session_id, session_id, "same-session compact");
                compact_boundary_seen = true;
            },
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "stop");
                assert_eq!(
                    event.session_id, session_id,
                    "turn completes on same session"
                );
                turn_completed = true;
                if compact_boundary_seen {
                    break;
                }
            },
            _ => {},
        }
    }
    assert_eq!(compaction_started_count, 1);
    assert!(compact_boundary_seen);

    let state = runtime
        .event_store
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(!state.context_messages.is_empty());
    assert!(state.messages.iter().any(|message| {
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
            .submit_input_for_session(sid.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }
    while event_rx.try_recv().is_ok() {}

    let error = handler.compact_session(sid.clone()).await.unwrap_err();
    assert!(error.to_string().contains("Compaction failed"));

    let state = runtime.event_store.session_read_model(&sid).await.unwrap();
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
