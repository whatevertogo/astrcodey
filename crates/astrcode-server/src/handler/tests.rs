use std::{
    collections::BTreeMap,
    fs, future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use astrcode_context::{compaction::CompactResult, context_assembler::LlmContextAssembler};
use astrcode_core::{
    config::{ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    event::{Event, EventPayload, Phase},
    extension::{
        CommandContext, CompactStrategy, Extension, ExtensionCommandResult, ExtensionError,
        ExtensionEvent, HookMode, HookResult, LifecycleContext, Registrar, SlashCommand,
    },
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    storage::EventStore,
    tool::{ToolDefinition, ToolResult},
    types::{SessionId, ToolCallId, new_session_id},
};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_session::{compact_boundary_payload, session_continued_from_compaction_payload};
use astrcode_storage::in_memory::InMemoryEventStore;
use astrcode_support::event_fanout::EventFanout;
use tokio::sync::mpsc;

use super::*;

struct MockLlm;
struct ReactiveCompactLlm {
    calls: AtomicUsize,
}
struct ExhaustedReactiveCompactLlm;
struct AutoCompactFailingLlm;

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

#[async_trait::async_trait]
impl LlmProvider for ReactiveCompactLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let is_compact_request = messages.last().is_some_and(|message| {
            message.role == LlmRole::User
                && message_to_dto(message)
                    .content
                    .contains("Do not call tools")
        });

        if is_compact_request {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: r#"<summary>
1. Primary Request and Intent:
   reactive compact summary

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
   reactive retry

9. Optional Next Step:
   - (none)
</summary>"#
                    .into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            return Ok(rx);
        }

        if call == 0 {
            return Err(LlmError::PromptTooLong("prompt too long".into()));
        }

        assert!(
            messages.iter().any(|message| message_to_dto(message)
                .content
                .contains("<compact_summary>")),
            "reactive retry should include compact summary"
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "reactive retry succeeded".into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 100,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ExhaustedReactiveCompactLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let is_compact_request = messages.last().is_some_and(|message| {
            message.role == LlmRole::User
                && message_to_dto(message)
                    .content
                    .contains("Do not call tools")
        });

        if is_compact_request {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: compact_summary_text("reactive compact summary"),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            return Ok(rx);
        }

        Err(LlmError::PromptTooLong("prompt too long".into()))
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 100,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for AutoCompactFailingLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let is_compact_request = messages.last().is_some_and(|message| {
            message.role == LlmRole::User
                && message_to_dto(message)
                    .content
                    .contains("Do not call tools")
        });
        if is_compact_request {
            return Err(LlmError::Transport("compact llm failed".into()));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "normal response".into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 100,
            max_output_tokens: 1024,
        }
    }
}

struct PendingLlm;
struct BlockFirstThenImmediateLlm {
    gate: Arc<tokio::sync::Notify>,
    calls: AtomicUsize,
}
struct DelayedLlm {
    started: tokio::sync::watch::Sender<bool>,
}
struct StreamErrorLlm;
struct ReadThenEditAcrossTurnsLlm {
    call_count: AtomicUsize,
}

struct FailSessionStartExtension;

struct RecordSessionResumeExtension {
    events: Arc<Mutex<Vec<ExtensionEvent>>>,
}

struct FailFirstSessionResumeExtension {
    calls: Arc<AtomicUsize>,
}

struct BlockingSessionResumeExtension {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
struct RecordingLifecycleExtension {
    events: Arc<Mutex<Vec<ExtensionEvent>>>,
}

#[derive(Clone, Default)]
struct CapturingLlm {
    messages: Arc<Mutex<Vec<LlmMessage>>>,
}

struct StaticCommandExtension {
    id: &'static str,
    command_name: &'static str,
}

#[async_trait::async_trait]
impl Extension for RecordingLifecycleExtension {
    fn id(&self) -> &str {
        "recording-lifecycle"
    }

    fn register(&self, reg: &mut Registrar) {
        for event in [
            ExtensionEvent::AfterProviderResponse,
            ExtensionEvent::TurnEnd,
        ] {
            reg.on_event(
                event.clone(),
                HookMode::Blocking,
                0,
                Arc::new(RecordingLifecycleHandler {
                    event,
                    events: Arc::clone(&self.events),
                }),
            );
        }
    }
}

struct RecordingLifecycleHandler {
    event: ExtensionEvent,
    events: Arc<Mutex<Vec<ExtensionEvent>>>,
}

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for RecordingLifecycleHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.events.lock().unwrap().push(self.event.clone());
        Ok(HookResult::Allow)
    }
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
            return Ok(ExtensionCommandResult::display("extension command", false));
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

#[async_trait::async_trait]
impl Extension for RecordSessionResumeExtension {
    fn id(&self) -> &str {
        "record-session-resume"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_event(
            ExtensionEvent::SessionResume,
            HookMode::Blocking,
            0,
            Arc::new(RecordingLifecycleHandler {
                event: ExtensionEvent::SessionResume,
                events: Arc::clone(&self.events),
            }),
        );
    }
}

#[async_trait::async_trait]
impl Extension for FailFirstSessionResumeExtension {
    fn id(&self) -> &str {
        "fail-first-session-resume"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_event(
            ExtensionEvent::SessionResume,
            HookMode::Blocking,
            0,
            Arc::new(FailFirstSessionResumeHandler {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl Extension for BlockingSessionResumeExtension {
    fn id(&self) -> &str {
        "blocking-session-resume"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_event(
            ExtensionEvent::SessionResume,
            HookMode::Blocking,
            0,
            Arc::new(BlockingSessionResumeHandler {
                calls: Arc::clone(&self.calls),
                entered: Arc::clone(&self.entered),
                release: Arc::clone(&self.release),
            }),
        );
    }
}

struct FailFirstSessionResumeHandler {
    calls: Arc<AtomicUsize>,
}

struct BlockingSessionResumeHandler {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for FailFirstSessionResumeHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ExtensionError::Internal("session resume failed".into()));
        }
        Ok(HookResult::Allow)
    }
}

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for BlockingSessionResumeHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(HookResult::Allow)
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
impl LlmProvider for BlockFirstThenImmediateLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.gate.notified().await;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: format!("reply-{call}"),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 8_192,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for DelayedLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let _ = self.started.send(true);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "late output".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for StreamErrorLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::Error {
            message: "stream failed".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ReadThenEditAcrossTurnsLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        match call {
            0 => {
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "read-call".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "note.txt" }).to_string(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "tool_calls".into(),
                });
            },
            1 => {
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: "read complete".into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            },
            2 => {
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "edit-call".into(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "path": "note.txt",
                        "oldStr": "alpha",
                        "newStr": "gamma"
                    })
                    .to_string(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "tool_calls".into(),
                });
            },
            _ => {
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: "edit complete".into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            },
        }
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
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        context: ContextSettings {
            auto_compact_enabled: context_settings.auto_compact_enabled,
            predictive_compact_enabled: context_settings.predictive_compact_enabled,
            compact_threshold_percent: context_settings.compact_threshold_percent,
            compact_max_retry_attempts: context_settings.compact_max_retry_attempts,
            compact_max_output_tokens: context_settings.compact_max_output_tokens,
            compact_keep_recent_turns: context_settings.compact_keep_recent_turns,
            predictive_compact_baseline_growth_tokens: context_settings
                .predictive_compact_baseline_growth_tokens,
            compact_circuit_breaker_threshold: context_settings.compact_circuit_breaker_threshold,
            compact_circuit_breaker_cooldown_secs: context_settings
                .compact_circuit_breaker_cooldown_secs,
            post_compact_max_files: context_settings.post_compact_max_files,
            post_compact_token_budget: context_settings.post_compact_token_budget,
            post_compact_max_tokens_per_file: context_settings.post_compact_max_tokens_per_file,
        },
        agent: astrcode_core::config::AgentSettings::default(),
        wasm: astrcode_core::config::WasmSettings::default(),
        extensions: ExtensionSettings::default(),
    };
    let event_store = Arc::new(InMemoryEventStore::new()) as Arc<dyn EventStore>;
    let extension_runner = Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
        Duration::from_secs(1),
    ));
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings.clone()));
    let capabilities = Arc::new(astrcode_session::SessionRuntimeServices::new(
        llm_provider.clone(),
        llm_provider,
        Arc::clone(&extension_runner),
        Arc::clone(&context_assembler),
        effective,
    ));
    let config = Arc::new(crate::config_manager::ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/test-config.json"),
        )),
        astrcode_core::config::Config::default(),
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(crate::session_manager::SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config),
        Arc::clone(&capabilities),
        vec![],
    ));
    Arc::new(ServerRuntime {
        event_store,
        config_manager: config,
        context_assembler,
        session_manager,
        extension_runner,
        capabilities,
        startup_working_dir: std::env::temp_dir(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
    })
}

fn test_runtime_with_llm(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
    test_runtime_with_settings(llm_provider, astrcode_context::ContextSettings::default())
}

fn test_runtime() -> Arc<ServerRuntime> {
    test_runtime_with_llm(Arc::new(MockLlm))
}

fn test_scheduler(runtime: &Arc<ServerRuntime>) -> Arc<crate::turn_scheduler::TurnScheduler> {
    Arc::new(crate::turn_scheduler::TurnScheduler::new(
        runtime.session_manager().clone(),
        Arc::new(crate::turn_registry::TurnRegistry::new()),
    ))
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

async fn recv_event(event_rx: &mut mpsc::Receiver<ClientNotification>) -> ClientNotification {
    tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("event should arrive")
        .expect("event channel should stay open")
}

fn test_event_bus(
    runtime: &Arc<crate::bootstrap::ServerRuntime>,
    event_tx: Arc<EventFanout<ClientNotification>>,
    scheduler: Arc<crate::turn_scheduler::TurnScheduler>,
) -> Arc<crate::server_event_bus::ServerEventBus> {
    let event_bus = Arc::new(crate::server_event_bus::ServerEventBus::new(
        event_tx, scheduler,
    ));
    runtime
        .session_manager()
        .bind_event_bus(Arc::clone(&event_bus));
    event_bus
}

fn spawn_test_actor(
    runtime: Arc<crate::bootstrap::ServerRuntime>,
    event_tx: Arc<EventFanout<ClientNotification>>,
) -> CommandHandle {
    let scheduler = test_scheduler(&runtime);
    CommandHandler::spawn_actor(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        test_event_bus(&runtime, event_tx, scheduler),
    )
}

fn compact_summary_text(current_work: &str) -> String {
    format!(
        r#"<summary>
1. Primary Request and Intent:
   compact test

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
   {current_work}

9. Optional Next Step:
   - (none)
</summary>"#
    )
}

async fn wait_for_turn_completed(event_rx: &mut mpsc::Receiver<ClientNotification>) -> String {
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
    event_rx: &mut mpsc::Receiver<ClientNotification>,
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

async fn append_user_assistant_pair(
    store: &Arc<dyn EventStore>,
    session_id: &SessionId,
    user: &str,
    assistant: &str,
) {
    store
        .append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: user.into(),
            },
        ))
        .await
        .unwrap();
    store
        .append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: assistant.into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();
}

async fn collect_turn_ids_until_completed(
    event_rx: &mut mpsc::Receiver<ClientNotification>,
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

    let boundary = compact_boundary_payload(
        "manual_command",
        &compaction,
        "child".into(),
        0,
        CompactStrategy::Manual {
            keep_recent_turns: None,
        },
    );
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
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock-model", None, None, None)
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();

    let event = Event::new(
        sid.clone(),
        None,
        EventPayload::SystemPromptConfigured {
            text: "ordered prompt".into(),
            fingerprint: "fingerprint".into(),
            extra_system_prompt: None,
        },
    );
    let event = runtime.event_store().append_event(event).await.unwrap();
    event_tx.send(ClientNotification::Event(event));

    let ClientNotification::Event(event) = recv_event(&mut event_rx).await else {
        panic!("expected event notification");
    };
    assert!(event.seq.is_some());

    let model = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(model.system_prompt.as_deref(), Some("ordered prompt"));
}

#[tokio::test]
async fn create_session_configures_system_prompt() {
    let runtime = test_runtime();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();

    let mut saw_configured = false;
    for _ in 0..2 {
        if let ClientNotification::Event(event) = recv_event(&mut event_rx).await {
            if let EventPayload::SystemPromptConfigured {
                text, fingerprint, ..
            } = event.payload
            {
                saw_configured = true;
                assert!(text.contains("[Identity]"));
                assert!(!fingerprint.is_empty());
            }
        }
    }
    assert!(saw_configured);

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
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
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

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
async fn reopening_persisted_session_emits_resume_once_per_runtime() {
    let runtime = test_runtime();
    let events = Arc::new(Mutex::new(Vec::new()));
    runtime
        .extension_runner
        .register(Arc::new(RecordSessionResumeExtension {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock-model", None, None, None)
        .await
        .unwrap();

    runtime.session_manager().open(sid.clone()).await.unwrap();
    runtime.session_manager().open(sid).await.unwrap();

    assert_eq!(*events.lock().unwrap(), vec![ExtensionEvent::SessionResume]);
}

#[tokio::test]
async fn failed_session_resume_is_retried_on_next_open() {
    let runtime = test_runtime();
    let calls = Arc::new(AtomicUsize::new(0));
    runtime
        .extension_runner
        .register(Arc::new(FailFirstSessionResumeExtension {
            calls: Arc::clone(&calls),
        }))
        .await
        .unwrap();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock-model", None, None, None)
        .await
        .unwrap();

    assert!(runtime.session_manager().open(sid.clone()).await.is_err());
    assert!(runtime.session_manager().open(sid).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_open_waits_for_initial_session_resume() {
    let runtime = test_runtime();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    runtime
        .extension_runner
        .register(Arc::new(BlockingSessionResumeExtension {
            calls: Arc::clone(&calls),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }))
        .await
        .unwrap();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock-model", None, None, None)
        .await
        .unwrap();

    let first_runtime = Arc::clone(&runtime);
    let first_sid = sid.clone();
    let first = tokio::spawn(async move { first_runtime.session_manager().open(first_sid).await });
    entered.notified().await;

    let second_runtime = Arc::clone(&runtime);
    let mut second = tokio::spawn(async move { second_runtime.session_manager().open(sid).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );

    release.notify_one();
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn submit_prompt_reuses_session_system_prompt() {
    let runtime = test_runtime();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    let initial_prompt = {
        let state = runtime
            .event_store()
            .session_read_model(&sid)
            .await
            .unwrap();
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

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(state.system_prompt, initial_prompt);
}

#[tokio::test]
async fn submit_prompt_configures_missing_session_system_prompt() {
    let runtime = test_runtime();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock-model", None, None, None)
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    handler
        .submit_input_for_session(sid.clone(), "hello".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(
        state
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("[Identity]"))
    );
}

#[tokio::test]
async fn submit_prompt_uses_one_turn_id_for_turn_events() {
    let runtime = test_runtime();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

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
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock", None, None, None)
        .await
        .unwrap();
    runtime
        .event_store()
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
    let stale_state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(stale_state.phase, Phase::CallingTool);
    assert!(
        stale_state
            .pending_tool_calls
            .contains(&ToolCallId::from("call-1"))
    );

    let event_tx = Arc::new(EventFanout::new(1024));
    let (actor_tx, _actor_rx) = mpsc::unbounded_channel();
    let scheduler = test_scheduler(&runtime);
    let handler = CommandHandler::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        test_event_bus(&runtime, event_tx, scheduler),
        actor_tx,
    );

    handler.repair_stale_session(&sid).await.unwrap();

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
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
async fn repair_stale_background_tasks_even_when_phase_is_idle() {
    let runtime = test_runtime();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(&sid, ".", "mock", None, None, None)
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            Some("turn-1".into()),
            EventPayload::ToolCallRequested {
                call_id: "call-bg".into(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({ "command": "long-running" }),
            },
        ))
        .await
        .unwrap();
    let mut metadata = BTreeMap::new();
    metadata.insert("task_id".into(), serde_json::json!("task-bg"));
    metadata.insert("backgrounded".into(), serde_json::json!(true));
    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            Some("turn-1".into()),
            EventPayload::ToolCallCompleted {
                call_id: "call-bg".into(),
                tool_name: "shell".into(),
                result: ToolResult {
                    call_id: "call-bg".into(),
                    content: "Task moved to background (task: task-bg).".into(),
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                },
                arguments: "long-running".into(),
                arguments_json: Some(serde_json::json!({ "command": "long-running" })),
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            Some("turn-1".into()),
            EventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        ))
        .await
        .unwrap();

    let stale_state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(stale_state.phase, Phase::Idle);
    assert!(
        !stale_state
            .background_tool_calls
            .get(&ToolCallId::from("call-bg"))
            .unwrap()
            .completed
    );

    let event_tx = Arc::new(EventFanout::new(1024));
    let (actor_tx, _actor_rx) = mpsc::unbounded_channel();
    let scheduler = test_scheduler(&runtime);
    let handler = CommandHandler::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        test_event_bus(&runtime, event_tx, scheduler),
        actor_tx,
    );

    handler.repair_stale_session(&sid).await.unwrap();

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(
        state
            .background_tool_calls
            .get(&ToolCallId::from("call-bg"))
            .unwrap()
            .completed
    );
    assert!(state.messages.iter().any(|message| {
        message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    is_error
                } if tool_call_id == "call-bg"
                    && *is_error
                    && content.contains("Background task interrupted")
            )
        })
    }));
}

#[tokio::test]
async fn repair_stale_runs_marks_child_without_active_execution_interrupted() {
    let runtime = test_runtime();
    let parent_id = new_session_id();
    let child_id = new_session_id();
    runtime
        .event_store()
        .create_session(&parent_id, ".", "mock", None, None, None)
        .await
        .unwrap();
    runtime
        .event_store()
        .create_session(&child_id, ".", "mock", Some(&parent_id), None, None)
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            parent_id.clone(),
            None,
            EventPayload::AgentSessionSpawned {
                child_session_id: child_id.clone(),
                agent_name: "explorer".into(),
                task: "inspect".into(),
                tool_policy: None,
                tool_call_id: "agent-call".into(),
            },
        ))
        .await
        .unwrap();

    let event_tx = Arc::new(EventFanout::new(1024));
    let (actor_tx, _actor_rx) = mpsc::unbounded_channel();
    let scheduler = test_scheduler(&runtime);
    let handler = CommandHandler::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        test_event_bus(&runtime, event_tx, scheduler),
        actor_tx,
    );

    handler.repair_stale_session(&parent_id).await.unwrap();

    let state = runtime
        .event_store()
        .session_read_model(&parent_id)
        .await
        .unwrap();
    let link = state.agent_sessions.first().unwrap();
    assert_eq!(
        link.status,
        astrcode_core::storage::AgentSessionStatus::Failed
    );
    assert_eq!(
        link.final_session_id.as_ref().map(|id| id.as_str()),
        Some(child_id.as_str())
    );
    assert_eq!(link.error.as_deref(), Some("interrupted"));
}

#[tokio::test]
async fn submit_prompt_queues_second_running_turn_for_next_turn() {
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let runtime = test_runtime_with_llm(Arc::new(PendingLlm));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "first".into())
        .await
        .unwrap();
    let queued = handler
        .submit_input_for_session(sid.clone(), "second".into())
        .await
        .unwrap();
    assert!(matches!(
        queued,
        PromptSubmission::Handled { message } if message == "queued for next turn"
    ));

    let mut saw_busy = false;
    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Error { code: 40900, .. } = notification {
            saw_busy = true;
            break;
        }
    }
    assert!(
        !saw_busy,
        "queued second prompt should not emit busy rejection error"
    );

    handler.abort_session(sid).await.unwrap();
}

#[tokio::test]
async fn queued_inputs_run_fifo_for_same_session() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let runtime = test_runtime_with_llm(Arc::new(BlockFirstThenImmediateLlm {
        gate: Arc::clone(&gate),
        calls: AtomicUsize::new(0),
    }));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    assert!(matches!(
        handler
            .submit_input_for_session(sid.clone(), "first".into())
            .await
            .unwrap(),
        PromptSubmission::Accepted { .. }
    ));
    assert!(matches!(
        handler
            .submit_input_for_session(sid.clone(), "second".into())
            .await
            .unwrap(),
        PromptSubmission::Handled { message } if message == "queued for next turn"
    ));
    assert!(matches!(
        handler
            .submit_input_for_session(sid.clone(), "third".into())
            .await
            .unwrap(),
        PromptSubmission::Handled { message } if message == "queued for next turn"
    ));

    gate.notify_one();

    for expected in ["stop", "stop", "stop"] {
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, expected);
    }

    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    let user_messages: Vec<String> = events
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_messages,
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );
}

#[tokio::test]
async fn successful_text_turn_dispatches_after_provider_response_before_turn_end() {
    let runtime = test_runtime_with_llm(Arc::new(CapturingLlm::default()));
    let events = Arc::new(Mutex::new(Vec::new()));
    runtime
        .extension_runner
        .register(Arc::new(RecordingLifecycleExtension {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let (_turn_id, completion) = handler
        .submit_prompt_with_completion(sid, "hello".into())
        .await
        .unwrap();
    let completion = completion.await.unwrap();

    assert!(matches!(completion, TurnCompletion::Completed { .. }));
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ExtensionEvent::AfterProviderResponse,
            ExtensionEvent::TurnEnd
        ]
    );
}

#[tokio::test]
async fn stream_error_still_dispatches_turn_end() {
    let runtime = test_runtime_with_llm(Arc::new(StreamErrorLlm));
    let events = Arc::new(Mutex::new(Vec::new()));
    runtime
        .extension_runner
        .register(Arc::new(RecordingLifecycleExtension {
            events: Arc::clone(&events),
        }))
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let (_turn_id, completion) = handler
        .submit_prompt_with_completion(sid, "hello".into())
        .await
        .unwrap();
    let completion = completion.await.unwrap();

    assert!(matches!(completion, TurnCompletion::Failed { .. }));
    assert_eq!(*events.lock().unwrap(), vec![ExtensionEvent::TurnEnd]);
}

#[tokio::test]
async fn read_before_edit_guard_survives_across_turns() {
    let workspace = unique_workspace("read-before-edit-cross-turn");
    let path = workspace.join("note.txt");
    fs::write(&path, "alpha").unwrap();
    let runtime = test_runtime_with_llm(Arc::new(ReadThenEditAcrossTurnsLlm {
        call_count: AtomicUsize::new(0),
    }));
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler
        .create_session(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();

    handler
        .submit_input_for_session(sid.clone(), "read the file".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    fs::write(&path, "beta").unwrap();

    handler
        .submit_input_for_session(sid.clone(), "edit the file".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    assert_eq!(fs::read_to_string(&path).unwrap(), "beta");
    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
                ..
            } if call_id.as_str() == "edit-call"
                && tool_name == "edit"
                && result.is_error
                && result.metadata.get("staleFile") == Some(&serde_json::json!(true))
        )
    }));
}

#[tokio::test]
async fn abort_stops_active_turn_and_records_completion() {
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let runtime = test_runtime_with_llm(Arc::new(PendingLlm));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();

    handler.abort_session(sid).await.unwrap();

    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
}

#[tokio::test]
async fn abort_stops_inner_turn_before_late_provider_events_are_persisted() {
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let runtime = test_runtime_with_llm(Arc::new(DelayedLlm {
        started: started_tx,
    }));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "start then abort".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started_rx.changed())
        .await
        .unwrap()
        .unwrap();

    handler.abort_session(sid.clone()).await.unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    assert!(!events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::AssistantMessageCompleted { text, .. } if text.contains("late output")
        )
    }));
}

#[tokio::test]
async fn compact_session_rejects_running_turn_without_compaction_started() {
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let runtime = test_runtime_with_llm(Arc::new(PendingLlm));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();
    while event_rx.try_recv().is_ok() {}

    let error = handler
        .compact_session(sid.clone(), None)
        .await
        .unwrap_err();
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
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let runtime = test_runtime_with_llm(Arc::new(PendingLlm));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

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
        .send(CommandMessage::AgentTurnCleanup {
            session_id: sid,
            turn_id,
            completion: TurnCompletion::Aborted,
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
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three"] {
        handler
            .submit_input_for_session(session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let compacted_id = handler
        .compact_session(session_id.clone(), None)
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
        .event_store()
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
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

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
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(
        state
            .messages
            .iter()
            .all(|message| message_to_dto(message).content != "/compact")
    );

    let following = handler
        .submit_input_for_session(session_id.clone(), "after compact".into())
        .await
        .unwrap();
    assert!(
        matches!(following, PromptSubmission::Accepted { .. }),
        "a completed slash compact must not leave later prompts queued"
    );
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
}

#[tokio::test]
async fn unknown_slash_command_falls_through_as_regular_prompt() {
    let runtime = test_runtime();
    let event_tx = Arc::new(EventFanout::new(1024));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    // /missing-command 不是已知斜杠命令，应作为普通 prompt 提交并启动 turn
    let result = handler
        .submit_input_for_session(sid.clone(), "/missing-command".into())
        .await;

    assert!(
        matches!(&result, Ok(PromptSubmission::Accepted { .. })),
        "expected Accepted, got {result:?}"
    );
}

#[tokio::test]
async fn skill_slash_command_uses_skill_content_as_user_message() {
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
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
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
    let user_text = captured
        .iter()
        .filter(|message| message.role == LlmRole::System)
        .map(|message| message_to_dto(message).content)
        .collect::<Vec<_>>()
        .join("\n");
    // system prompt 不应包含 skill 内容
    assert!(!user_text.contains("<skill-name>reviewnow</skill-name>"));

    // skill 内容直接作为 user message 发给 LLM
    let user_messages: Vec<_> = captured
        .iter()
        .filter(|message| message.role == LlmRole::User)
        .map(|message| message_to_dto(message).content.clone())
        .collect();
    assert!(
        user_messages
            .iter()
            .any(|text| text.contains("<skill-name>reviewnow</skill-name>")),
        "skill content should be sent as user message: {user_messages:?}"
    );

    // transcript 记录的是 skill 展开后的内容（统一路径，与 agent 实际接收一致）
    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(
        state.messages.iter().any(|message| message_to_dto(message)
            .content
            .contains("<skill-name>reviewnow</skill-name>")),
        "transcript should contain resolved skill content"
    );
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn command_list_keeps_reserved_and_extension_priority_over_skills() {
    let workspace = unique_workspace("slash-command-priority");
    write_project_skill(
        &workspace,
        "compact",
        "---\ndescription: Skill named compact.\n---\nShould never override builtin.",
    );
    write_project_skill(
        &workspace,
        "reviewnow",
        "---\ndescription: Skill named reviewnow.\n---\nShould not override extension.",
    );
    let runtime = test_runtime();
    runtime
        .extension_runner
        .register(astrcode_extension_skill::extension())
        .await
        .unwrap();
    runtime
        .extension_runner
        .register(Arc::new(StaticCommandExtension {
            id: "test-extension",
            command_name: "reviewnow",
        }))
        .await
        .unwrap();
    let event_tx = Arc::new(EventFanout::new(1024));
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
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
    assert_eq!(reviewnow.source, "extension");
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn compact_command_compacts_existing_hidden_context_again() {
    let settings = astrcode_context::ContextSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for text in ["one", "two", "three", "four"] {
        handler
            .submit_input_for_session(session_id.clone(), text.into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    }

    let first_compacted = handler
        .compact_session(session_id.clone(), None)
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
            .event_store()
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
        .compact_session(session_id.clone(), None)
        .await
        .map(compacted_session_id)
        .unwrap();
    assert_eq!(second_compacted, session_id, "same-session compact again");
    assert_eq!(
        session_id,
        drain_until_compact_boundary(&mut event_rx).await
    );

    let state = runtime
        .event_store()
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
async fn auto_compact_applies_in_memory_during_turn() {
    let settings = astrcode_context::ContextSettings {
        compact_threshold_percent: 0.0,
        ..Default::default()
    };
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .event_store()
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
            .event_store()
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
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
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
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "stop");
                assert_eq!(
                    event.session_id, session_id,
                    "turn completes on same session"
                );
                break;
            },
            _ => {},
        }
    }
    assert_eq!(compaction_started_count, 1);
}

#[tokio::test]
async fn prompt_too_long_triggers_reactive_compact_and_retries_once() {
    let runtime = test_runtime_with_settings(
        Arc::new(ReactiveCompactLlm {
            calls: AtomicUsize::new(0),
        }),
        astrcode_context::ContextSettings {
            auto_compact_enabled: false,
            ..Default::default()
        },
    );
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .event_store()
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
            .event_store()
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

    let mut saw_compaction_started = 0usize;
    let mut saw_compaction_completed = 0usize;
    loop {
        let notification = recv_event(&mut event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::CompactionStarted => saw_compaction_started += 1,
            EventPayload::CompactionCompleted { .. } => saw_compaction_completed += 1,
            EventPayload::AssistantMessageCompleted { text, .. }
                if text == "reactive retry succeeded" =>
            {
                break;
            },
            _ => {},
        }
    }

    assert_eq!(saw_compaction_started, 1);
    assert_eq!(saw_compaction_completed, 1);
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(!state.context_messages.is_empty());
}

#[tokio::test]
async fn prompt_too_long_after_reactive_retry_returns_compact_exhausted() {
    let runtime = test_runtime_with_settings(
        Arc::new(ExhaustedReactiveCompactLlm),
        astrcode_context::ContextSettings {
            auto_compact_enabled: false,
            ..Default::default()
        },
    );
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    append_user_assistant_pair(
        runtime.event_store(),
        &session_id,
        "old user",
        "old assistant",
    )
    .await;

    handler
        .submit_input_for_session(session_id, "current".into())
        .await
        .unwrap();

    let mut saw_compaction_completed = false;
    let mut saw_compact_exhausted = false;
    loop {
        let notification = recv_event(&mut event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::CompactionCompleted { .. } => saw_compaction_completed = true,
            EventPayload::ErrorOccurred { message, .. } => {
                saw_compact_exhausted =
                    message.contains("prompt is still too long after reactive compaction");
            },
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "error");
                break;
            },
            _ => {},
        }
    }

    assert!(saw_compaction_completed);
    assert!(saw_compact_exhausted);
}

#[tokio::test]
async fn auto_compact_uses_configured_keep_recent_turns() {
    let runtime = test_runtime_with_settings(
        Arc::new(MockLlm),
        astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            compact_keep_recent_turns: Some(2),
            ..Default::default()
        },
    );
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        append_user_assistant_pair(
            runtime.event_store(),
            &session_id,
            &format!("old user {index}"),
            &format!("old assistant {index}"),
        )
        .await;
    }

    handler
        .submit_input_for_session(session_id.clone(), "current".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

    let state = runtime
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    let visible = state
        .messages
        .iter()
        .map(|message| message_to_dto(message).content)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!visible.contains("old user 0"));
    assert!(!visible.contains("old user 1"));
    assert!(visible.contains("old user 2"));
    assert!(visible.contains("current"));
    assert!(matches!(
        state
            .compact_boundaries
            .first()
            .map(|boundary| &boundary.strategy),
        Some(CompactStrategy::Auto)
    ));
}

#[tokio::test]
async fn auto_compact_breaker_skips_llm_but_still_runs_deterministic_compact() {
    let runtime = test_runtime_with_settings(
        Arc::new(AutoCompactFailingLlm),
        astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            compact_circuit_breaker_threshold: 1,
            compact_circuit_breaker_cooldown_secs: 60,
            ..Default::default()
        },
    );
    let event_tx = Arc::new(EventFanout::new(1024));
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    append_user_assistant_pair(
        runtime.event_store(),
        &session_id,
        "old user",
        "old assistant",
    )
    .await;

    handler
        .submit_input_for_session(session_id.clone(), "first".into())
        .await
        .unwrap();

    let mut first_compactions = 0usize;
    loop {
        let notification = recv_event(&mut event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::CompactionStarted => first_compactions += 1,
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "stop");
                break;
            },
            _ => {},
        }
    }
    assert_eq!(first_compactions, 1);

    handler
        .submit_input_for_session(session_id, "second".into())
        .await
        .unwrap();

    let mut second_compactions = 0usize;
    loop {
        let notification = recv_event(&mut event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::CompactionStarted => second_compactions += 1,
            EventPayload::TurnCompleted { finish_reason } => {
                assert_eq!(finish_reason, "stop");
                break;
            },
            _ => {},
        }
    }
    // 断路器只阻止再次调用 LLM，阈值仍满足时会做确定性 compact。
    assert_eq!(second_compactions, 1);
}
