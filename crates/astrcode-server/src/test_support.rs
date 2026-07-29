//! 集成测试构造内部编排组件（需 `testing` feature）。

pub use crate::{
    child_session::ChildSessionCoordinator,
    config_manager::ConfigManager,
    server_event_bus::ServerEventBus,
    session_manager::SessionManager,
    session_operations::ServerSessionOperations,
    turn_registry::TurnRegistry,
    turn_scheduler::{
        DeliveryOutcome, InputDelivery, MAX_PENDING_INPUTS_PER_SESSION, MAX_PROMPT_TEXT_BYTES,
        StartedExecution, TurnScheduleError, TurnScheduler,
    },
};

pub fn session_started_event_for_test(
    session_id: astrcode_core::types::SessionId,
    working_dir: impl Into<String>,
    model_id: impl Into<String>,
) -> astrcode_core::event::DurableEvent {
    use astrcode_core::event::{
        DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted,
        SystemPromptSource,
    };

    DurableEvent::session(
        session_id,
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: working_dir.into(),
            model_id: model_id.into(),
            parent: None,
            tool_selection: Default::default(),
            source_extension: None,
            initial_system_prompt: PersistedSystemPrompt {
                text: "test system prompt".into(),
                fingerprint: "test-system-prompt".into(),
                extra_system_prompt: None,
                source: SystemPromptSource::Native,
            },
        }),
    )
}

pub fn child_session_started_event_for_test(
    session_id: astrcode_core::types::SessionId,
    working_dir: impl Into<String>,
    model_id: impl Into<String>,
    parent_session_id: astrcode_core::types::SessionId,
) -> astrcode_core::event::DurableEvent {
    let mut event = session_started_event_for_test(session_id, working_dir, model_id);
    let astrcode_core::event::DurableEventPayload::SessionStarted(started) = &mut event.payload
    else {
        unreachable!("test session event is always SessionStarted");
    };
    started.parent = Some(astrcode_core::event::ParentSessionRef {
        session_id: parent_session_id,
    });
    event
}

pub fn assemble_session_runtime_services_for_test(
    llm: std::sync::Arc<dyn astrcode_core::llm::LlmProvider>,
    small_llm: std::sync::Arc<dyn astrcode_core::llm::LlmProvider>,
    effective: astrcode_core::config::EffectiveConfig,
    extension_runner: std::sync::Arc<astrcode_extensions::runner::ExtensionRunner>,
    context_assembler: std::sync::Arc<astrcode_context::context_assembler::LlmContextAssembler>,
    shell_timeout_secs: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> std::sync::Arc<astrcode_session::SessionRuntimeServices> {
    crate::config_manager::assemble_session_runtime_services(
        llm,
        small_llm,
        effective,
        extension_runner,
        context_assembler,
        shell_timeout_secs,
    )
}

pub async fn recycle_completed_session_for_test(
    scheduler: &TurnScheduler,
    session_id: &astrcode_core::types::SessionId,
    turn_id: &astrcode_core::types::TurnId,
) -> Result<bool, TurnScheduleError> {
    scheduler
        .recycle_completed_session(session_id, turn_id)
        .await
        .map(|outcome| {
            matches!(
                outcome,
                crate::turn_scheduler::CompletedRecycleOutcome::Recycled
            )
        })
}
