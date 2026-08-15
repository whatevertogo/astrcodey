use std::{
    fs, future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use astrcode_context::is_compact_summary_message;
use astrcode_core::{
    compaction::CompactStrategy,
    config::{
        ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme,
        ProviderWireFormat,
    },
    event::{DurableEvent, DurableEventPayload, EventPayload, LiveEventPayload, Phase},
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    types::{SessionId, ToolCallId, new_session_id},
};
use astrcode_extension_sdk::{
    builder::{command, manifest},
    extension::{
        CommandAvailability, CommandCompletionContext, CommandCompletions, CommandContext,
        ExtensionCapability, ExtensionCommandResult, ExtensionError, ExtensionManifest, HookMode,
        HookResult, LifecycleContext, LifecycleEvent, Registrar, SessionCommandIntent,
        SessionCommandKind,
    },
};
use astrcode_extensions::{Extension, testing::extension_runner_with_extensions};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_session_projection::SessionReadModel;
use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};
use tokio::sync::{broadcast, mpsc};

use super::*;

fn test_extension_manifest(id: impl Into<String>) -> ExtensionManifest {
    manifest(id)
        .version("test")
        .description("Server handler test extension")
        .build()
}

trait ProviderMessages {
    fn provider_messages(&self) -> Vec<LlmMessage>;
}

impl ProviderMessages for SessionReadModel {
    fn provider_messages(&self) -> Vec<LlmMessage> {
        astrcode_core::llm::provider_visible_messages(
            self.model_context
                .messages
                .iter()
                .map(|message| (*message.message).clone())
                .collect(),
        )
    }
}

struct MockLlm;
struct ReactiveCompactLlm {
    calls: AtomicUsize,
}
struct ExhaustedReactiveCompactLlm;
#[derive(Default)]
struct AutoCompactFailingLlm {
    compact_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for MockLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let messages = request.messages;
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
            return Err(LlmError::ContextWindowExceeded {
                message: "prompt too long".into(),
            });
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
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ExhaustedReactiveCompactLlm {
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let messages = request.messages;
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

        Err(LlmError::ContextWindowExceeded {
            message: "prompt too long".into(),
        })
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for AutoCompactFailingLlm {
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let messages = request.messages;
        let is_compact_request = messages.last().is_some_and(|message| {
            message.role == LlmRole::User
                && message_to_dto(message)
                    .content
                    .contains("Do not call tools")
        });
        if is_compact_request {
            self.compact_calls.fetch_add(1, Ordering::SeqCst);
            return Err(LlmError::transport("compact llm failed"));
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
            max_input_tokens: 200000,
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

struct FailingSessionStartObserver {
    calls: Arc<AtomicUsize>,
}

struct RecordSessionResumeExtension {
    events: Arc<Mutex<Vec<LifecycleEvent>>>,
}

struct FailingSessionResumeObserver {
    calls: Arc<AtomicUsize>,
}

struct AwaitedSessionResumeObserver {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
struct RecordingLifecycleExtension {
    events: Arc<Mutex<Vec<LifecycleEvent>>>,
}

#[derive(Clone, Default)]
struct CapturingLlm {
    messages: Arc<Mutex<Vec<LlmMessage>>>,
}

struct StaticCommandExtension {
    id: &'static str,
    command_name: &'static str,
}

struct InteractiveCommandProbeExtension {
    execute_calls: Arc<AtomicUsize>,
    completion_calls: Arc<AtomicUsize>,
}

struct InteractiveCommandProbeHandler {
    execute_calls: Arc<AtomicUsize>,
    completion_calls: Arc<AtomicUsize>,
}

struct BusyCompactProbeExtension {
    execute_calls: Arc<AtomicUsize>,
}

struct BusyCompactProbeHandler {
    execute_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Extension for RecordingLifecycleExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("recording-lifecycle")
    }

    fn register(&self, reg: &mut Registrar) {
        for event in [
            LifecycleEvent::AfterProviderResponse,
            LifecycleEvent::TurnEnd,
        ] {
            reg.on_lifecycle(
                event.clone(),
                HookMode::Advisory,
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
    event: LifecycleEvent,
    events: Arc<Mutex<Vec<LifecycleEvent>>>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::LifecycleHandler for RecordingLifecycleHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.events.lock().unwrap().push(self.event.clone());
        Ok(HookResult::Allow)
    }
}

#[async_trait::async_trait]
impl Extension for StaticCommandExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest(self.id)
    }

    fn register(&self, reg: &mut Registrar) {
        let command_name = self.command_name;
        reg.command(
            command(command_name)
                .description("Static test command")
                .priority(10)
                .build(),
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
impl astrcode_extension_sdk::extension::CommandHandler for StaticCommandHandler {
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError> {
        if ctx.command_name() == self.command_name {
            return Ok(ExtensionCommandResult::display("extension command", false));
        }
        Err(ExtensionError::NotFound(ctx.command_name().into()))
    }
}

#[async_trait::async_trait]
impl Extension for InteractiveCommandProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("interactive-command-probe")
    }

    fn register(&self, reg: &mut Registrar) {
        reg.command(
            command("interactive-probe")
                .description("Interactive command admission probe")
                .argument_completions(true)
                .availability(CommandAvailability::InteractiveOnly)
                .build(),
            Arc::new(InteractiveCommandProbeHandler {
                execute_calls: Arc::clone(&self.execute_calls),
                completion_calls: Arc::clone(&self.completion_calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::CommandHandler for InteractiveCommandProbeHandler {
    async fn execute(
        &self,
        _ctx: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExtensionCommandResult::handled("interactive probe handled"))
    }

    async fn complete(
        &self,
        _ctx: CommandCompletionContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        self.completion_calls.fetch_add(1, Ordering::SeqCst);
        Ok(CommandCompletions::default())
    }

    fn supports_argument_completions(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl Extension for BusyCompactProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("busy-compact-probe")
            .version("test")
            .description("Busy compact admission probe")
            .capability(ExtensionCapability::SessionCommand)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        reg.command(
            command("compact")
                .description("Busy compact admission probe")
                .requires_idle(true)
                .priority(200)
                .host_command(SessionCommandKind::CompactSession)
                .build(),
            Arc::new(BusyCompactProbeHandler {
                execute_calls: Arc::clone(&self.execute_calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::CommandHandler for BusyCompactProbeHandler {
    async fn execute(
        &self,
        _ctx: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExtensionCommandResult::host_command(
            SessionCommandIntent::CompactSession {
                keep_recent_turns: None,
            },
        ))
    }
}

#[async_trait::async_trait]
impl Extension for FailingSessionStartObserver {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("failing-session-start-observer")
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            LifecycleEvent::SessionStart,
            HookMode::Advisory,
            0,
            Arc::new(FailingSessionStartHandler {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl Extension for RecordSessionResumeExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("record-session-resume")
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            LifecycleEvent::SessionResume,
            HookMode::Advisory,
            0,
            Arc::new(RecordingLifecycleHandler {
                event: LifecycleEvent::SessionResume,
                events: Arc::clone(&self.events),
            }),
        );
    }
}

#[async_trait::async_trait]
impl Extension for FailingSessionResumeObserver {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("failing-session-resume-observer")
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            LifecycleEvent::SessionResume,
            HookMode::Advisory,
            0,
            Arc::new(FailingSessionResumeHandler {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl Extension for AwaitedSessionResumeObserver {
    fn manifest(&self) -> ExtensionManifest {
        test_extension_manifest("awaited-session-resume-observer")
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            LifecycleEvent::SessionResume,
            HookMode::Advisory,
            0,
            Arc::new(AwaitedSessionResumeHandler {
                calls: Arc::clone(&self.calls),
                entered: Arc::clone(&self.entered),
                release: Arc::clone(&self.release),
            }),
        );
    }
}

struct FailingSessionResumeHandler {
    calls: Arc<AtomicUsize>,
}

struct AwaitedSessionResumeHandler {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::LifecycleHandler for FailingSessionResumeHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ExtensionError::Internal("session resume failed".into()));
        }
        Ok(HookResult::Allow)
    }
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::LifecycleHandler for AwaitedSessionResumeHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(HookResult::Allow)
    }
}

struct FailingSessionStartHandler {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::LifecycleHandler for FailingSessionStartHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ExtensionError::Internal("session start failed".into()))
    }
}

#[async_trait::async_trait]
impl LlmProvider for PendingLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
            max_input_tokens: 200000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for StreamErrorLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
                        "oldText": "alpha",
                        "newText": "gamma"
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
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let messages = request.messages;
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
    test_runtime_with_runner(
        llm_provider,
        context_settings,
        Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
            Duration::from_secs(1),
        )),
    )
}

/// 扩展必须在 SessionRuntimeServices 之前完成装配，否则 runner generation
/// 与 expected epoch 不匹配,turn 执行会以 RuntimeUnstable 失败。
async fn test_runtime_with_extensions(
    llm_provider: Arc<dyn LlmProvider>,
    extensions: Vec<Arc<dyn Extension>>,
) -> Arc<ServerRuntime> {
    let extension_runner =
        extension_runner_with_extensions(Duration::from_secs(1), None, extensions)
            .await
            .expect("assemble test extension runner");
    test_runtime_with_runner(
        llm_provider,
        astrcode_context::ContextSettings::default(),
        extension_runner,
    )
}

fn test_runtime_with_runner(
    llm_provider: Arc<dyn LlmProvider>,
    context_settings: astrcode_context::ContextSettings,
    extension_runner: Arc<astrcode_extensions::runner::ExtensionRunner>,
) -> Arc<ServerRuntime> {
    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
        },
        small_llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
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
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    let event_store = Arc::new(InMemoryEventStore::new()) as Arc<dyn SessionStore>;
    let runtime_services = crate::config_manager::assemble_session_runtime_services(
        llm_provider.clone(),
        llm_provider,
        effective,
        extension_runner.clone(),
    );
    let config = Arc::new(crate::config_manager::ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/test-config.toml"),
        )),
        astrcode_core::config::Config::default(),
        Arc::clone(&extension_runner),
        Arc::clone(&runtime_services),
        std::path::PathBuf::from("."),
    ));
    let session_manager = Arc::new(crate::session_manager::SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&runtime_services),
        vec![],
    ));
    let child_sessions = Arc::new(crate::child_session::ChildSessionCoordinator::new(
        Arc::clone(&session_manager),
    ));
    let scheduler = Arc::new(crate::turn_scheduler::TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(crate::turn_registry::TurnRegistry::new()),
        child_sessions,
    ));
    Arc::new(ServerRuntime {
        event_store,
        config_manager: config,
        session_manager,
        scheduler,
        extension_runner,
        runtime_services,
        transport_profile: Default::default(),
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

fn coding_extension() -> Arc<dyn Extension> {
    astrcode_bundled_extensions::bundled_extensions(&Default::default())
        .into_iter()
        .find(|extension| extension.manifest().id() == "astrcode-coding")
        .expect("coding extension is included in server test features")
}

fn session_commands_extension() -> Arc<dyn Extension> {
    astrcode_bundled_extensions::bundled_extensions(&Default::default())
        .into_iter()
        .find(|extension| extension.manifest().id() == "astrcode-session-commands")
        .expect("session command extension is included in server test features")
}

fn test_scheduler(runtime: &Arc<ServerRuntime>) -> Arc<crate::turn_scheduler::TurnScheduler> {
    let session_manager = runtime.session_manager().clone();
    let child_sessions = Arc::new(crate::child_session::ChildSessionCoordinator::new(
        Arc::clone(&session_manager),
    ));
    Arc::new(crate::turn_scheduler::TurnScheduler::new(
        session_manager,
        Arc::new(crate::turn_registry::TurnRegistry::new()),
        child_sessions,
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

fn assert_compacted(outcome: ManualCompactionOutcome) {
    if let ManualCompactionOutcome::Skipped { message } = outcome {
        panic!("expected compact, compact was skipped: {message}");
    }
}

fn event_channel(capacity: usize) -> broadcast::Sender<ClientNotification> {
    broadcast::channel(capacity).0
}

async fn recv_event(event_rx: &mut broadcast::Receiver<ClientNotification>) -> ClientNotification {
    tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("event should arrive")
        .expect("event channel should stay open")
}

fn test_event_bus(
    runtime: &Arc<crate::bootstrap::ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> Arc<crate::server_event_bus::ServerEventBus> {
    let event_bus = Arc::clone(runtime.session_manager().event_bus());
    let mut notifications = event_bus.subscribe_all_notifications();
    tokio::spawn(async move {
        while let Ok(notification) = notifications.recv().await {
            let _ = event_tx.send(notification);
        }
    });
    event_bus
}

struct TestCommandActor {
    handle: CommandHandle,
    session_commands: crate::session_command_service::SessionCommandService,
    event_bus: Arc<crate::server_event_bus::ServerEventBus>,
}

impl TestCommandActor {
    async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        self.handle.handle(command).await
    }

    async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        let mut events = self.event_bus.subscribe_all_notifications();
        self.handle
            .handle(ClientCommand::CreateSession { working_dir })
            .await?;
        loop {
            let notification = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .map_err(|_| HandlerError::ActorUnavailable)?
                .map_err(|_| HandlerError::ActorUnavailable)?;
            if let ClientNotification::Event(event) = notification
                && matches!(
                    event.payload,
                    EventPayload::Durable(DurableEventPayload::SessionStarted(_))
                )
            {
                return Ok(event.session_id);
            }
        }
    }

    async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        input: astrcode_core::user_input::UserInput,
    ) -> Result<
        (
            astrcode_core::types::TurnId,
            tokio::sync::oneshot::Receiver<TurnCompletion>,
        ),
        HandlerError,
    > {
        self.session_commands
            .submit_input_with_completion(session_id, input)
            .await
    }

    async fn submit_input_for_session(
        &self,
        session_id: SessionId,
        input: astrcode_core::user_input::UserInput,
    ) -> Result<PromptSubmission, HandlerError> {
        self.session_commands.submit_input(session_id, input).await
    }

    async fn compact_session(
        &self,
        session_id: SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactionOutcome, HandlerError> {
        self.session_commands
            .compact_session(&session_id, keep_recent_turns)
            .await
    }

    async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        self.session_commands.abort_session(&session_id).await
    }

    async fn command_list_for_session(
        &self,
        session_id: SessionId,
        include_interactive: bool,
    ) -> Result<CommandList, HandlerError> {
        self.session_commands
            .command_list(&session_id, include_interactive)
            .await
    }

    async fn invoke_command_for_session(
        &self,
        session_id: SessionId,
        command_name: String,
        arguments: String,
    ) -> Result<CommandInvocation, HandlerError> {
        self.session_commands
            .invoke_named_command(session_id, command_name, arguments)
            .await
    }

    async fn complete_command_for_session(
        &self,
        session_id: SessionId,
        command_name: String,
        argument: String,
    ) -> Result<CommandCompletions, HandlerError> {
        self.session_commands
            .complete_command(session_id, command_name, argument, None)
            .await
    }
}

fn spawn_test_actor(
    runtime: Arc<crate::bootstrap::ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> TestCommandActor {
    let scheduler = test_scheduler(&runtime);
    let event_bus = test_event_bus(&runtime, event_tx);
    let session_commands = crate::session_command_service::SessionCommandService::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );
    let handle = CommandHandler::spawn_actor(
        Arc::clone(&runtime),
        scheduler,
        Arc::clone(&event_bus),
        session_commands.clone(),
    );
    TestCommandActor {
        handle,
        session_commands,
        event_bus,
    }
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

async fn wait_for_turn_completed(event_rx: &mut broadcast::Receiver<ClientNotification>) -> String {
    loop {
        let notification = recv_event(event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        if let EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) =
            event.payload
        {
            return finish_reason;
        }
    }
}

async fn drain_until_transcript_rewrite(
    event_rx: &mut broadcast::Receiver<ClientNotification>,
) -> SessionId {
    loop {
        let notification = recv_event(event_rx).await;
        let ClientNotification::Event(event) = notification else {
            continue;
        };
        if matches!(
            event.payload,
            EventPayload::Durable(DurableEventPayload::TranscriptRewritten { .. })
        ) {
            return event.session_id;
        }
    }
}

async fn append_user_assistant_pair(
    store: &Arc<dyn SessionStore>,
    session_id: &SessionId,
    user: &str,
    assistant: &str,
) {
    store
        .append_event(DurableEvent::new(
            session_id.clone(),
            None,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: user.into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();
    store
        .append_event(DurableEvent::new(
            session_id.clone(),
            None,
            DurableEventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: assistant.into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();
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
            EventPayload::Durable(
                DurableEventPayload::TurnStarted
                | DurableEventPayload::UserMessage { .. }
                | DurableEventPayload::AssistantMessageCompleted { .. },
            ) => {
                turn_ids.push(event.turn_id);
            },
            EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
                turn_ids.push(event.turn_id);
                return (finish_reason, turn_ids);
            },
            _ => {},
        }
    }
}

async fn wait_until_no_active_turn(
    scheduler: &crate::turn_scheduler::TurnScheduler,
    session_id: &SessionId,
) {
    for _ in 0..50 {
        if !scheduler.registry().has_active(session_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("turn registry entry was not cleaned up");
}

#[tokio::test]
async fn record_and_broadcast_updates_projection_before_broadcast() {
    let runtime = test_runtime();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock-model",
        ))
        .await
        .unwrap();
    let event_tx = event_channel(1024);
    let mut event_rx = event_tx.subscribe();

    let event = DurableEvent::new(
        sid.clone(),
        None,
        DurableEventPayload::SystemPromptConfigured {
            text: "ordered prompt".into(),
            fingerprint: "fingerprint".into(),
            extra_system_prompt: None,
            source: Default::default(),
        },
    );
    let event = runtime.event_store().append_event(event).await.unwrap();
    event_tx
        .send(ClientNotification::Event(event.into()))
        .unwrap();

    let ClientNotification::Event(event) = recv_event(&mut event_rx).await else {
        panic!("expected event notification");
    };
    assert!(event.seq.is_some());

    let model = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(model.system_prompt.text, "ordered prompt");
}

#[tokio::test]
async fn create_session_persists_initial_system_prompt() {
    let runtime = test_runtime();
    let event_tx = event_channel(1024);
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();

    let ClientNotification::Event(event) = recv_event(&mut event_rx).await else {
        panic!("expected session start event");
    };
    let EventPayload::Durable(DurableEventPayload::SessionStarted(started)) = event.payload else {
        panic!("session start should contain its initial prompt");
    };
    let prompt = started.initial_system_prompt;
    assert!(prompt.text.contains("[Identity]"));
    assert!(!prompt.fingerprint.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), event_rx.recv())
            .await
            .is_err(),
        "successful creation should publish SessionStarted exactly once"
    );

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(state.system_prompt.text.contains("[Identity]"));
    assert!(state.model_context.messages.is_empty());
}

#[tokio::test]
async fn client_create_session_ignores_start_observer_failure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(FailingSessionStartObserver {
            calls: Arc::clone(&calls),
        })],
    )
    .await;
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    handler
        .handle(ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .expect("observer failure must not block session creation");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reopening_persisted_session_emits_resume_once_per_runtime() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(RecordSessionResumeExtension {
            events: Arc::clone(&events),
        })],
    )
    .await;
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock-model",
        ))
        .await
        .unwrap();

    runtime.session_manager().open(sid.clone()).await.unwrap();
    runtime.session_manager().open(sid).await.unwrap();

    assert_eq!(*events.lock().unwrap(), vec![LifecycleEvent::SessionResume]);
}

#[tokio::test]
async fn failed_session_resume_observer_does_not_fail_or_repeat_open() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(FailingSessionResumeObserver {
            calls: Arc::clone(&calls),
        })],
    )
    .await;
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock-model",
        ))
        .await
        .unwrap();

    assert!(runtime.session_manager().open(sid.clone()).await.is_ok());
    assert!(runtime.session_manager().open(sid).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_open_waits_for_initial_session_resume() {
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(AwaitedSessionResumeObserver {
            calls: Arc::clone(&calls),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })],
    )
    .await;
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock-model",
        ))
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
async fn cancelled_initial_resume_allows_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(AwaitedSessionResumeObserver {
            calls: Arc::clone(&calls),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })],
    )
    .await;
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock-model",
        ))
        .await
        .unwrap();

    let first_runtime = Arc::clone(&runtime);
    let first_sid = sid.clone();
    let first = tokio::spawn(async move { first_runtime.session_manager().open(first_sid).await });
    entered.notified().await;
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));

    release.notify_one();
    let reopened =
        tokio::time::timeout(Duration::from_secs(1), runtime.session_manager().open(sid))
            .await
            .unwrap();

    assert!(reopened.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn submit_prompt_reuses_session_system_prompt() {
    let runtime = test_runtime();
    let event_tx = event_channel(1024);
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
async fn submit_prompt_uses_one_turn_id_for_turn_events() {
    let runtime = test_runtime();
    let event_tx = event_channel(1024);
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
async fn startup_repairs_stale_pending_tool_calls() {
    let runtime = test_runtime();
    let sid = new_session_id();
    let clean_sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            clean_sid.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            Some("stale-turn".into()),
            DurableEventPayload::ToolCallRequested {
                call_id: "call-1".into(),
                tool_name: "todoWrite".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
            },
        ))
        .await
        .unwrap();
    let stale_state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(stale_state.execution.phase, Phase::CallingTool);
    assert!(
        stale_state
            .execution
            .pending_tool_calls
            .contains(&ToolCallId::from("call-1"))
    );

    let app = crate::bootstrap::ServerApp::new(Arc::clone(&runtime));
    app.initialize().await;

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(state.execution.phase, Phase::Idle);
    assert!(state.execution.pending_tool_calls.is_empty());
    assert!(state.model_context.messages.iter().any(|message| {
        message.message.content.iter().any(|content| {
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
    assert!(
        state
            .provider_messages()
            .iter()
            .any(|message| { message.joined_display_text("").contains("<turn_aborted>") })
    );
    app.shutdown().await;
}

#[tokio::test]
async fn repair_stale_session_settles_dangling_tool_call_after_aborted_turn() {
    let runtime = test_runtime();
    let sid = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            sid.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            Some("aborted-turn".into()),
            DurableEventPayload::ToolCallRequested {
                call_id: "call-abort".into(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({ "command": "sleep" }),
                raw_arguments: None,
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            Some("aborted-turn".into()),
            DurableEventPayload::TurnCompleted {
                finish_reason: "aborted".into(),
            },
        ))
        .await
        .unwrap();

    let stale_state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert_eq!(stale_state.execution.phase, Phase::Idle);
    assert!(stale_state.execution.pending_tool_calls.is_empty());

    let event_tx = event_channel(1024);
    let scheduler = test_scheduler(&runtime);
    let event_bus = test_event_bus(&runtime, event_tx);
    let session_commands = crate::session_command_service::SessionCommandService::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );
    let handler = CommandHandler::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        event_bus,
        session_commands,
    );

    handler.repair_stale_session(&sid).await.unwrap();

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(state.model_context.messages.iter().any(|message| {
        message.message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    is_error
                } if tool_call_id == "call-abort"
                    && *is_error
                    && content.contains("interrupted before completion")
            )
        })
    }));
    assert!(
        state
            .provider_messages()
            .iter()
            .any(|message| { message.joined_display_text("").contains("<turn_aborted>") })
    );
}

#[tokio::test]
async fn repair_stale_runs_marks_child_without_active_execution_interrupted() {
    let runtime = test_runtime();
    let parent_id = new_session_id();
    let child_id = new_session_id();
    runtime
        .event_store()
        .create_session(crate::test_support::session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .create_session(crate::test_support::child_session_started_event_for_test(
            child_id.clone(),
            ".",
            "mock",
            parent_id.clone(),
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            parent_id.clone(),
            None,
            DurableEventPayload::AgentSessionSpawned {
                child_session_id: child_id.clone(),
                agent_name: "explorer".into(),
                task: "inspect".into(),
                tool_selection: None,
                tool_call_id: Some("agent-call".into()),
            },
        ))
        .await
        .unwrap();

    let event_tx = event_channel(1024);
    let scheduler = test_scheduler(&runtime);
    let event_bus = test_event_bus(&runtime, event_tx);
    let session_commands = crate::session_command_service::SessionCommandService::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );
    let handler = CommandHandler::new(
        Arc::clone(&runtime),
        Arc::clone(&scheduler),
        event_bus,
        session_commands,
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
        astrcode_session_projection::AgentSessionStatus::Failed
    );
    assert_eq!(
        link.final_session_id.as_ref().map(|id| id.as_str()),
        Some(child_id.as_str())
    );
    assert_eq!(link.error.as_deref(), Some("interrupted"));
}

#[tokio::test]
async fn submit_prompt_queues_second_running_turn_for_next_turn() {
    let event_tx = event_channel(1024);
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
async fn queue_input_started_from_idle_is_cleaned_up() {
    let runtime = test_runtime_with_llm(Arc::new(MockLlm));
    let scheduler = test_scheduler(&runtime);
    let created = runtime.session_manager().create(".").await.unwrap();
    let sid = created.id().clone();

    let outcome = scheduler
        .deliver_input(
            sid.clone(),
            "queued-after-race".into(),
            crate::turn_scheduler::InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        crate::turn_scheduler::DeliveryOutcome::Started { .. }
    ));
    wait_until_no_active_turn(&scheduler, &sid).await;
    assert_eq!(
        runtime
            .event_store()
            .session_read_model(&sid)
            .await
            .unwrap()
            .execution
            .phase,
        Phase::Idle
    );
}

#[tokio::test]
async fn queued_inputs_run_fifo_for_same_session() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let event_tx = event_channel(1024);
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
        .filter_map(|event| match event.event.payload {
            DurableEventPayload::UserMessage { text, .. } => Some(text),
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let runtime = test_runtime_with_extensions(
        Arc::new(CapturingLlm::default()),
        vec![Arc::new(RecordingLifecycleExtension {
            events: Arc::clone(&events),
        })],
    )
    .await;
    let event_tx = event_channel(1024);
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
            LifecycleEvent::AfterProviderResponse,
            LifecycleEvent::TurnEnd
        ]
    );
}

#[tokio::test]
async fn stream_error_still_dispatches_turn_end() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let runtime = test_runtime_with_extensions(
        Arc::new(StreamErrorLlm),
        vec![Arc::new(RecordingLifecycleExtension {
            events: Arc::clone(&events),
        })],
    )
    .await;
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let (_turn_id, completion) = handler
        .submit_prompt_with_completion(sid, "hello".into())
        .await
        .unwrap();
    let completion = completion.await.unwrap();

    assert!(matches!(completion, TurnCompletion::Failed { .. }));
    assert_eq!(*events.lock().unwrap(), vec![LifecycleEvent::TurnEnd]);
}

#[tokio::test]
async fn read_before_edit_guard_survives_across_turns() {
    let workspace = unique_workspace("read-before-edit-cross-turn");
    let path = workspace.join("note.txt");
    fs::write(&path, "alpha").unwrap();
    let runtime = test_runtime_with_extensions(
        Arc::new(ReadThenEditAcrossTurnsLlm {
            call_count: AtomicUsize::new(0),
        }),
        vec![coding_extension()],
    )
    .await;
    let event_tx = event_channel(1024);
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
            DurableEventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
                ..
            } if call_id.as_str() == "edit-call"
                && tool_name == "edit"
                && result.is_error
                && result.metadata.get("errorCode")
                    == Some(&serde_json::json!(
                        astrcode_extension_sdk::WireErrorCode::StaleFile.as_str()
                    ))
                && result.metadata.get("errorDetails").and_then(|details| details.get("reason"))
                    == Some(&serde_json::json!("changed"))
        )
    }));
}

#[tokio::test]
async fn abort_stops_active_turn_and_records_completion() {
    let event_tx = event_channel(1024);
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
async fn server_shutdown_stops_active_turn_before_draining_watchers() {
    let runtime = test_runtime_with_llm(Arc::new(PendingLlm));
    let app = crate::bootstrap::ServerApp::new(runtime);
    let session_id = app
        .session_commands()
        .create_session(".".into(), None)
        .await
        .unwrap();
    let submission = app
        .session_commands()
        .submit_input(session_id, "keep running".into())
        .await
        .unwrap();
    assert!(matches!(submission, PromptSubmission::Accepted { .. }));

    tokio::time::timeout(Duration::from_secs(4), app.shutdown())
        .await
        .expect("shutdown should not wait forever for an active turn");
}

#[tokio::test]
async fn abort_stops_inner_turn_before_late_provider_events_are_persisted() {
    let event_tx = event_channel(1024);
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
            DurableEventPayload::AssistantMessageCompleted { text, .. }
                if text.contains("late output")
        )
    }));
}

#[tokio::test]
async fn slash_compact_rejects_running_turn_without_input_or_compaction_events() {
    let event_tx = event_channel(1024);
    let mut event_rx = event_tx.subscribe();
    let compact_handler_calls = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime_with_extensions(
        Arc::new(PendingLlm),
        vec![
            session_commands_extension(),
            Arc::new(BusyCompactProbeExtension {
                execute_calls: Arc::clone(&compact_handler_calls),
            }),
        ],
    )
    .await;
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let sid = handler.create_session(".".into()).await.unwrap();
    handler
        .submit_input_for_session(sid.clone(), "keep running".into())
        .await
        .unwrap();
    while event_rx.try_recv().is_ok() {}

    let error = handler
        .submit_input_for_session(sid.clone(), "/COMPACT".into())
        .await
        .unwrap_err();
    assert!(
        matches!(error, HandlerError::CompactBlocked),
        "expected CompactBlocked, got {error:?}"
    );

    while let Ok(notification) = event_rx.try_recv() {
        if let ClientNotification::Event(event) = notification {
            assert!(
                !matches!(
                    event.payload,
                    EventPayload::Live(LiveEventPayload::CompactionStarted)
                ),
                "rejected compact must not leave clients in compacting state"
            );
        }
    }

    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    assert!(events.iter().all(|event| {
        !matches!(&event.payload, DurableEventPayload::UserMessage { text, .. } if text.eq_ignore_ascii_case("/compact"))
            && !matches!(
                &event.payload,
                DurableEventPayload::UserInputAccepted { input }
                    if input.text.eq_ignore_ascii_case("/compact")
            )
    }));
    assert_eq!(compact_handler_calls.load(Ordering::SeqCst), 0);

    handler.abort_session(sid).await.unwrap();
}

#[tokio::test]
async fn compact_command_rewrites_transcript_with_summary() {
    let settings = astrcode_context::ContextSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let event_tx = event_channel(1024);
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

    handler
        .compact_session(session_id.clone(), None)
        .await
        .map(assert_compacted)
        .unwrap();
    let rewritten_session_id = drain_until_transcript_rewrite(&mut event_rx).await;
    assert_eq!(rewritten_session_id, session_id);

    let state = runtime
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(
        state
            .provider_messages()
            .iter()
            .any(is_compact_summary_message)
    );
    assert!(state.provider_messages().iter().any(|message| {
        message_to_dto(message)
            .content
            .contains("<compact_summary>")
    }));
    assert_eq!(
        state
            .model_context
            .messages
            .iter()
            .filter(|message| is_compact_summary_message(&message.message))
            .count(),
        1
    );
}

#[tokio::test]
async fn slash_compact_uses_backend_command_without_user_message() {
    let runtime =
        test_runtime_with_extensions(Arc::new(MockLlm), vec![session_commands_extension()]).await;
    let event_tx = event_channel(1024);
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
    let rewritten_session_id = drain_until_transcript_rewrite(&mut event_rx).await;
    assert_eq!(rewritten_session_id, session_id, "same-session compact");

    let state = runtime
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    assert!(
        state
            .model_context
            .messages
            .iter()
            .all(|message| message_to_dto(&message.message).content != "/compact")
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
async fn unknown_slash_command_is_rejected_without_writing_user_input() {
    let runtime = test_runtime();
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let error = handler
        .submit_input_for_session(sid.clone(), "/missing-command".into())
        .await
        .unwrap_err();
    assert!(matches!(error, HandlerError::UnknownCommand(name) if name == "missing-command"));

    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    assert!(events.iter().all(|event| {
        !matches!(&event.payload, DurableEventPayload::UserMessage { text, .. } if text == "/missing-command")
            && !matches!(
                &event.payload,
                DurableEventPayload::UserInputAccepted { input }
                    if input.text == "/missing-command"
            )
    }));
}

#[tokio::test]
async fn empty_slash_falls_through_as_regular_prompt() {
    let runtime = test_runtime();
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let result = handler.submit_input_for_session(sid, "/".into()).await;

    assert!(
        matches!(&result, Ok(PromptSubmission::Accepted { .. })),
        "expected Accepted, got {result:?}"
    );
}

#[tokio::test]
async fn extension_display_slash_command_returns_content_in_handled_message() {
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(StaticCommandExtension {
            id: "test-extension",
            command_name: "demo-cmd",
        })],
    )
    .await;
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let result = handler
        .submit_input_for_session(sid, "/demo-cmd".into())
        .await
        .unwrap();

    assert!(
        matches!(
            &result,
            PromptSubmission::Handled { message } if message == "extension command"
        ),
        "expected display content in Handled message, got {result:?}"
    );
}

#[tokio::test]
async fn invoke_command_normalizes_name_at_session_boundary() {
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![Arc::new(StaticCommandExtension {
            id: "test-extension",
            command_name: "demo-cmd",
        })],
    )
    .await;
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler.create_session(".".into()).await.unwrap();

    let result = handler
        .invoke_command_for_session(sid, " /DEMO-CMD ".into(), String::new())
        .await
        .unwrap();

    assert!(
        matches!(
            &result,
            CommandInvocation::Display { content, is_error: false } if content == "extension command"
        ),
        "expected normalized command invoke, got {result:?}"
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
    let runtime =
        test_runtime_with_extensions(Arc::new(llm), vec![astrcode_extension_skill::extension()])
            .await;
    let event_tx = event_channel(1024);
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
        .map(|message| message_to_dto(message).content)
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
        state
            .model_context
            .messages
            .iter()
            .any(|message| message_to_dto(&message.message)
                .content
                .contains("<skill-name>reviewnow</skill-name>")),
        "transcript should contain resolved skill content"
    );
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn session_commands_share_extension_resolution_and_transport_admission() {
    let workspace = unique_workspace("slash-command-priority");
    write_project_skill(
        &workspace,
        "compact",
        "---\ndescription: Skill named compact.\n---\nShould not override an extension.",
    );
    write_project_skill(
        &workspace,
        "reviewnow",
        "---\ndescription: Skill named reviewnow.\n---\nShould not override extension.",
    );
    let interactive_execute_calls = Arc::new(AtomicUsize::new(0));
    let interactive_completion_calls = Arc::new(AtomicUsize::new(0));
    let runtime = test_runtime_with_extensions(
        Arc::new(MockLlm),
        vec![
            session_commands_extension(),
            astrcode_extension_skill::extension(),
            Arc::new(StaticCommandExtension {
                id: "test-extension",
                command_name: "reviewnow",
            }),
            Arc::new(InteractiveCommandProbeExtension {
                execute_calls: Arc::clone(&interactive_execute_calls),
                completion_calls: Arc::clone(&interactive_completion_calls),
            }),
        ],
    )
    .await;
    let event_tx = event_channel(1024);
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);
    let sid = handler
        .create_session(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();

    let interactive_commands = handler
        .command_list_for_session(sid.clone(), true)
        .await
        .unwrap()
        .commands;
    let noninteractive_commands = handler
        .command_list_for_session(sid.clone(), false)
        .await
        .unwrap()
        .commands;

    let compact_commands = interactive_commands
        .iter()
        .filter(|command| command.name == "compact")
        .collect::<Vec<_>>();
    assert_eq!(compact_commands.len(), 1);
    assert_eq!(
        compact_commands[0].extension_id,
        "astrcode-session-commands"
    );
    let reviewnow = interactive_commands
        .iter()
        .find(|command| command.name == "reviewnow")
        .expect("reviewnow command");
    assert_eq!(reviewnow.extension_id, "test-extension");
    assert!(
        interactive_commands
            .iter()
            .any(|command| command.name == "model")
    );
    assert!(
        interactive_commands
            .iter()
            .any(|command| command.name == "interactive-probe")
    );
    assert!(
        noninteractive_commands
            .iter()
            .all(|command| command.name != "model" && command.name != "interactive-probe")
    );
    assert!(
        noninteractive_commands
            .iter()
            .any(|command| command.name == "compact")
    );

    for command_name in ["model", "interactive-probe"] {
        let error = handler
            .invoke_command_for_session(sid.clone(), command_name.into(), String::new())
            .await
            .unwrap_err();
        assert!(matches!(error, HandlerError::InvalidRequest(_)));
    }
    let completion_error = handler
        .complete_command_for_session(sid.clone(), "interactive-probe".into(), String::new())
        .await
        .unwrap_err();
    assert!(matches!(completion_error, HandlerError::InvalidRequest(_)));
    assert_eq!(interactive_execute_calls.load(Ordering::SeqCst), 0);
    assert_eq!(interactive_completion_calls.load(Ordering::SeqCst), 0);

    let compact_argument_error = handler
        .invoke_command_for_session(sid.clone(), "compact".into(), "not-a-number".into())
        .await
        .unwrap_err();
    assert!(matches!(
        compact_argument_error,
        HandlerError::InvalidRequest(_)
    ));

    let mut notifications = handler.event_bus.subscribe_all_notifications();
    handler
        .handle(ClientCommand::ExecuteExtensionCommand {
            command_name: "model".into(),
            arguments: String::new(),
        })
        .await
        .unwrap();
    loop {
        let notification = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .expect("model selector notification should arrive")
            .expect("notification channel should stay open");
        if let ClientNotification::UiRequest { request_id, .. } = notification {
            assert_eq!(request_id, "model.target");
            break;
        }
    }
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn compact_command_compacts_existing_hidden_context_again() {
    let settings = astrcode_context::ContextSettings::default();
    let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
    let event_tx = event_channel(1024);
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

    handler
        .compact_session(session_id.clone(), None)
        .await
        .map(assert_compacted)
        .unwrap();
    assert_eq!(
        session_id,
        drain_until_transcript_rewrite(&mut event_rx).await
    );
    let first_summary = {
        let state = runtime
            .event_store()
            .session_read_model(&session_id)
            .await
            .unwrap();
        let messages = state.provider_messages();
        message_to_dto(
            messages
                .iter()
                .find(|message| is_compact_summary_message(message))
                .expect("compact summary"),
        )
        .content
    };

    handler
        .submit_input_for_session(session_id.clone(), "five".into())
        .await
        .unwrap();
    assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
    handler
        .compact_session(session_id.clone(), None)
        .await
        .map(assert_compacted)
        .unwrap();
    assert_eq!(
        session_id,
        drain_until_transcript_rewrite(&mut event_rx).await
    );

    let state = runtime
        .event_store()
        .session_read_model(&session_id)
        .await
        .unwrap();
    let messages = state.provider_messages();
    let second_summary = message_to_dto(
        messages
            .iter()
            .find(|message| is_compact_summary_message(message))
            .expect("compact summary"),
    )
    .content;
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
    let event_tx = event_channel(1024);
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                session_id.clone(),
                None,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(20)),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                session_id.clone(),
                None,
                DurableEventPayload::AssistantMessageCompleted {
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
            EventPayload::Live(LiveEventPayload::CompactionStarted) => {
                compaction_started_count += 1;
                assert_eq!(event.session_id, session_id);
            },
            EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
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
    let event_tx = event_channel(1024);
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(Arc::clone(&runtime), event_tx);

    let session_id = handler.create_session(".".into()).await.unwrap();
    for index in 0..3 {
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                session_id.clone(),
                None,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(20)),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                session_id.clone(),
                None,
                DurableEventPayload::AssistantMessageCompleted {
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
            EventPayload::Live(LiveEventPayload::CompactionStarted) => {
                saw_compaction_started += 1;
            },
            EventPayload::Live(LiveEventPayload::CompactionCompleted { .. }) => {
                saw_compaction_completed += 1;
            },
            EventPayload::Durable(DurableEventPayload::AssistantMessageCompleted {
                text, ..
            }) if text == "reactive retry succeeded" => {
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
    assert!(
        state
            .provider_messages()
            .iter()
            .any(is_compact_summary_message)
    );
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
    let event_tx = event_channel(1024);
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
            EventPayload::Live(LiveEventPayload::CompactionCompleted { .. }) => {
                saw_compaction_completed = true;
            },
            EventPayload::Durable(DurableEventPayload::ErrorOccurred { message, .. })
            | EventPayload::Live(LiveEventPayload::ErrorOccurred { message, .. }) => {
                saw_compact_exhausted =
                    message.contains("prompt is still too long after reactive compaction");
            },
            EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
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
    let event_tx = event_channel(1024);
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
        .model_context
        .messages
        .iter()
        .map(|message| message_to_dto(&message.message).content)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!visible.contains("old user 0"));
    assert!(!visible.contains("old user 1"));
    assert!(visible.contains("old user 2"));
    assert!(visible.contains("current"));
    assert!(matches!(
        state
            .model_context
            .compactions
            .first()
            .map(|compaction| &compaction.strategy),
        Some(CompactStrategy::Auto)
    ));
}

#[tokio::test]
async fn auto_compact_breaker_skips_llm_but_still_runs_deterministic_compact() {
    let llm = Arc::new(AutoCompactFailingLlm::default());
    let runtime = test_runtime_with_settings(
        llm.clone(),
        astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            compact_circuit_breaker_threshold: 1,
            compact_circuit_breaker_cooldown_secs: 60,
            ..Default::default()
        },
    );
    let event_tx = event_channel(1024);
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
            EventPayload::Live(LiveEventPayload::CompactionStarted) => first_compactions += 1,
            EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
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
            EventPayload::Live(LiveEventPayload::CompactionStarted) => second_compactions += 1,
            EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
                assert_eq!(finish_reason, "stop");
                break;
            },
            _ => {},
        }
    }
    // 断路器只阻止再次调用 LLM，阈值仍满足时会做确定性 compact。
    assert_eq!(second_compactions, 1);
    assert_eq!(
        llm.compact_calls.load(Ordering::SeqCst),
        1,
        "the second turn must reuse the session-scoped open breaker"
    );
}

// ─── 流式工具调用测试 ──────────────────────────────────────────────────

/// Mock LLM：发送两个 read 工具调用，并在两个调用都 start 后再发送完成信号。
/// 第二次调用（工具结果反馈后）返回文本完成。
struct StreamingToolCallLlm {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for StreamingToolCallLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        match call {
            0 => {
                // 两个工具调用先后开始；OpenAI Chat Completions 的 done-marker fallback
                // 也可能形成这种“全部 start 后再 completed”的事件顺序。
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "stream-read-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "Cargo.toml" }).to_string(),
                });
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "stream-read-2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "README.md" }).to_string(),
                });

                let _ = tx.send(LlmEvent::ToolCallCompleted {
                    call_id: "stream-read-1".into(),
                });
                let _ = tx.send(LlmEvent::ToolCallCompleted {
                    call_id: "stream-read-2".into(),
                });

                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "tool_calls".into(),
                });
            },
            _ => {
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: "done".into(),
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

/// 验证带 `ToolCallCompleted` 事件的流式工具调用能正确执行并提交结果。
#[tokio::test]
async fn streaming_tool_call_completed_executes_tools() {
    let workspace = unique_workspace("streaming-tool-call");
    fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"test\"\n").unwrap();
    fs::write(workspace.join("README.md"), "# test\n").unwrap();

    let llm = Arc::new(StreamingToolCallLlm {
        call_count: AtomicUsize::new(0),
    });
    let runtime = test_runtime_with_extensions(llm, vec![coding_extension()]).await;
    let event_tx = event_channel(128);
    let mut event_rx = event_tx.subscribe();
    let handler = spawn_test_actor(runtime.clone(), event_tx);

    let sid = handler
        .create_session(workspace.to_string_lossy().into_owned())
        .await
        .unwrap();

    handler
        .submit_input_for_session(sid.clone(), "read both files".into())
        .await
        .unwrap();

    let finish_reason = wait_for_turn_completed(&mut event_rx).await;
    assert_eq!(finish_reason, "stop");

    // 验证两个工具调用都被执行并提交了 durable 事件
    let events = runtime.event_store().replay_events(&sid).await.unwrap();
    let completed_tool_calls: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::ToolCallCompleted { call_id, .. }
                    if call_id.as_str() == "stream-read-1"
                        || call_id.as_str() == "stream-read-2"
            )
        })
        .collect();
    assert_eq!(
        completed_tool_calls.len(),
        2,
        "both streaming tool calls should be committed"
    );
}
