//! Complete server application ownership.

use std::sync::Arc;

use tokio::sync::OnceCell;

use super::ServerRuntime;
use crate::{
    handler::{CommandHandle, CommandHandler},
    server_event_bus::ServerEventBus,
    session_command_service::SessionCommandService,
    session_manager::SessionManagerError,
    turn_scheduler::TurnScheduler,
};

/// Fully initialized server application shared by all transports.
///
/// A [`ServerRuntime`] owns durable services. `ServerApp` adds the single
/// process-wide event bus, command controller, and shutdown lifecycle. Build it
/// once, then pass the same instance to HTTP, ACP, or an in-process transport.
pub struct ServerApp {
    runtime: Arc<ServerRuntime>,
    event_bus: Arc<ServerEventBus>,
    session_commands: SessionCommandService,
    command_handle: CommandHandle,
    initialized: OnceCell<()>,
    shutdown: OnceCell<()>,
}

impl ServerApp {
    pub fn new(runtime: Arc<ServerRuntime>) -> Arc<Self> {
        let event_bus = Arc::clone(runtime.session_manager().event_bus());
        let session_commands = SessionCommandService::new(
            Arc::clone(&runtime),
            Arc::clone(runtime.scheduler()),
            Arc::clone(&event_bus),
        );
        let command_handle = CommandHandler::spawn_actor(
            Arc::clone(&runtime),
            Arc::clone(runtime.scheduler()),
            Arc::clone(&event_bus),
            session_commands.clone(),
        );
        Arc::new(Self {
            runtime,
            event_bus,
            session_commands,
            command_handle,
            initialized: OnceCell::new(),
            shutdown: OnceCell::new(),
        })
    }

    pub fn runtime(&self) -> &Arc<ServerRuntime> {
        &self.runtime
    }

    pub fn event_bus(&self) -> &Arc<ServerEventBus> {
        &self.event_bus
    }

    pub fn command_handle(&self) -> &CommandHandle {
        &self.command_handle
    }

    pub(crate) fn session_commands(&self) -> &SessionCommandService {
        &self.session_commands
    }

    pub(crate) fn scheduler(&self) -> &Arc<TurnScheduler> {
        self.runtime.scheduler()
    }

    pub fn request_shutdown(&self) {
        self.runtime.shutdown_token().cancel();
    }

    /// Repair durable turn phases left behind by a previous process before any
    /// transport starts serving requests.
    pub async fn initialize(&self) {
        if let Err(error) = self
            .initialized
            .get_or_try_init(|| async {
                let summaries = self.runtime.session_manager().list_summaries().await?;
                for summary in summaries {
                    let state = match self
                        .runtime
                        .session_manager()
                        .read_model(&summary.session_id)
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => {
                            tracing::warn!(
                                session_id = %summary.session_id,
                                %error,
                                "failed to inspect session during startup repair"
                            );
                            continue;
                        },
                    };
                    if !TurnScheduler::needs_stale_repair(&state) {
                        continue;
                    }
                    if let Err(error) = self
                        .session_commands
                        .repair_stale_session(&summary.session_id)
                        .await
                    {
                        tracing::warn!(
                            session_id = %summary.session_id,
                            %error,
                            "failed to repair stale session during startup"
                        );
                    }
                }
                Ok::<(), SessionManagerError>(())
            })
            .await
        {
            tracing::warn!(%error, "failed to enumerate sessions during startup repair");
        }
    }

    /// Stop the application exactly once; concurrent callers wait for the same
    /// shutdown sequence.
    pub async fn shutdown(&self) {
        self.shutdown
            .get_or_init(|| async {
                self.request_shutdown();
                self.command_handle.shutdown().await;
                self.scheduler().shutdown_background_tasks().await;
                self.runtime.shutdown_extensions().await;
                self.runtime.session_manager().shutdown_event_sink().await;
            })
            .await;
    }
}
