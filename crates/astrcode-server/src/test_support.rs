//! 集成测试构造内部编排组件（需 `testing` feature）。

use std::sync::Arc;

use astrcode_extensions::runner::ExtensionRunner;
use astrcode_storage::SessionStore;
use axum::Router;

use crate::{ServerEventBus, bootstrap::ServerRuntime, http::HttpServerError};
pub use crate::{
    child_session::ChildSessionCoordinator,
    config_manager::ConfigManager,
    session_manager::SessionManager,
    session_operations::ServerSessionOperations,
    turn_registry::TurnRegistry,
    turn_scheduler::{
        DeliveryOutcome, InputDelivery, MAX_PENDING_INPUTS_PER_SESSION, MAX_PROMPT_TEXT_BYTES,
        StartedExecution, TurnScheduleError, TurnScheduler,
    },
};

/// Test-only access to runtime components that are private to server orchestration.
pub trait ServerRuntimeTestExt {
    fn event_store(&self) -> &Arc<dyn SessionStore>;
    fn config_manager(&self) -> &Arc<ConfigManager>;
    fn extension_runner(&self) -> &Arc<ExtensionRunner>;
}

impl ServerRuntimeTestExt for ServerRuntime {
    fn event_store(&self) -> &Arc<dyn SessionStore> {
        ServerRuntime::event_store(self)
    }

    fn config_manager(&self) -> &Arc<ConfigManager> {
        ServerRuntime::config_manager(self)
    }

    fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        ServerRuntime::extension_runner(self)
    }
}

pub fn router_with_event_bus(
    server_app: Arc<crate::bootstrap::ServerApp>,
) -> Result<(Router, String, Arc<ServerEventBus>), HttpServerError> {
    let event_bus = Arc::clone(server_app.event_bus());
    let (router, auth_token) = crate::http::router(server_app)?;
    Ok((router, auth_token, event_bus))
}

/// Mirrors the production bootstrap host-router wiring so integration tests load
/// bundled extensions against the same backends instead of re-deriving them.
pub fn bind_extension_host_router_for_test(
    runner: &Arc<ExtensionRunner>,
    runtime_services: &astrcode_session::SessionRuntimeServices,
    session_store: Arc<dyn SessionStore>,
    cwd: &std::path::Path,
) {
    crate::bootstrap::bind_extension_host_router(runner, runtime_services, session_store, cwd);
}

/// Builds a runtime from already assembled test components.
#[allow(clippy::too_many_arguments)] // Mirrors the runtime's owned components.
pub fn assemble_server_runtime(
    event_store: Arc<dyn SessionStore>,
    config_manager: Arc<ConfigManager>,
    session_manager: Arc<SessionManager>,
    scheduler: Arc<TurnScheduler>,
    extension_runner: Arc<ExtensionRunner>,
    runtime_services: Arc<astrcode_session::SessionRuntimeServices>,
    startup_working_dir: std::path::PathBuf,
) -> ServerRuntime {
    ServerRuntime {
        event_store,
        config_manager,
        session_manager,
        scheduler,
        extension_runner,
        runtime_services,
        transport_profile: Default::default(),
        startup_working_dir,
        shutdown_token: tokio_util::sync::CancellationToken::new(),
    }
}

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
) -> std::sync::Arc<astrcode_session::SessionRuntimeServices> {
    crate::config_manager::assemble_session_runtime_services(
        llm,
        small_llm,
        effective,
        extension_runner,
    )
}

pub async fn recycle_completed_session_for_test(
    scheduler: &TurnScheduler,
    session_id: &astrcode_core::types::SessionId,
    turn_id: &astrcode_core::types::TurnId,
) -> Result<bool, TurnScheduleError> {
    scheduler
        .recycle_completed_session(session_id, turn_id, None)
        .await
        .map(|outcome| {
            matches!(
                outcome,
                crate::turn_scheduler::CompletedRecycleOutcome::Recycled
            )
        })
}

pub async fn release_completed_execution_for_test(
    scheduler: &TurnScheduler,
    session_id: &astrcode_core::types::SessionId,
    turn_id: &astrcode_core::types::TurnId,
    finalization: Option<&astrcode_session::TurnFinalization>,
) {
    scheduler
        .release_completed_execution(session_id, turn_id, finalization)
        .await;
}

pub async fn finish_and_watch_next_for_test(
    scheduler: &TurnScheduler,
    session_id: &astrcode_core::types::SessionId,
    turn_id: &astrcode_core::types::TurnId,
    finalization: Option<&astrcode_session::TurnFinalization>,
) -> Result<bool, TurnScheduleError> {
    match scheduler
        .finish_and_maybe_start_next(session_id, turn_id, finalization)
        .await?
    {
        crate::turn_scheduler::FinishOutcome::Settled(next) => {
            scheduler.watch_queued_if_any(session_id.clone(), next);
            Ok(true)
        },
        crate::turn_scheduler::FinishOutcome::Stale => Ok(false),
    }
}

pub async fn start_with_completion_and_hold_operation_for_test(
    scheduler: &TurnScheduler,
    session_id: astrcode_core::types::SessionId,
    input: astrcode_core::user_input::UserInput,
    started: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> Result<StartedExecution, TurnScheduleError> {
    let operation = scheduler.begin_session_operation(&session_id).await?;
    let execution = scheduler
        .start_with_completion_in_operation(&operation, input)
        .await?;
    let _ = started.send(());
    let _ = release.await;
    drop(operation);
    Ok(execution)
}

pub async fn start_with_completion_for_test(
    scheduler: &TurnScheduler,
    session_id: astrcode_core::types::SessionId,
    input: astrcode_core::user_input::UserInput,
) -> Result<crate::turn_scheduler::StartedExecution, crate::turn_scheduler::TurnScheduleError> {
    scheduler.start_with_completion(session_id, input).await
}

pub fn pause_next_completion_guard_registration_for_test(
    coordinator: &ChildSessionCoordinator,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    coordinator.pause_next_registration()
}

pub fn pause_next_completion_guard_claim_for_test(
    coordinator: &ChildSessionCoordinator,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    coordinator.pause_next_claim()
}

pub fn pause_next_sync_completion_settled_for_test(
    coordinator: &ChildSessionCoordinator,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    coordinator.pause_next_sync_settled()
}

pub fn registered_completion_guard_count_for_test(
    coordinator: &ChildSessionCoordinator,
    parent_session_id: &astrcode_core::types::SessionId,
) -> usize {
    coordinator.registered_guard_count(parent_session_id)
}

pub fn completed_completion_guard_count_for_test(
    coordinator: &ChildSessionCoordinator,
    parent_session_id: &astrcode_core::types::SessionId,
) -> usize {
    coordinator.completed_guard_count(parent_session_id)
}
