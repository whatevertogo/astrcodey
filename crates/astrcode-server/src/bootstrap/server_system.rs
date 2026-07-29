//! Server 核心系统组装 — 事件总线 + scheduler + handler actor。

use std::sync::Arc;

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

use super::ServerRuntime;
use crate::{
    handler::{CommandHandle, CommandHandler},
    server_event_bus::ServerEventBus,
    turn_scheduler::TurnScheduler,
};

pub struct ServerSystem {
    pub event_bus: Arc<ServerEventBus>,
    pub handler: CommandHandle,
    pub scheduler: Arc<TurnScheduler>,
}

pub fn spawn_server_system(
    runtime: &Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> ServerSystem {
    spawn_server_system_with_legacy(runtime, Some(event_tx))
}

pub fn spawn_server_system_without_legacy(runtime: &Arc<ServerRuntime>) -> ServerSystem {
    spawn_server_system_with_legacy(runtime, None)
}

fn spawn_server_system_with_legacy(
    runtime: &Arc<ServerRuntime>,
    event_tx: Option<broadcast::Sender<ClientNotification>>,
) -> ServerSystem {
    let scheduler = Arc::clone(runtime.scheduler());

    let event_bus = match event_tx {
        Some(event_tx) => Arc::new(ServerEventBus::with_legacy_tx(event_tx)),
        None => Arc::new(ServerEventBus::new()),
    };

    runtime
        .session_manager()
        .bind_event_bus(Arc::clone(&event_bus));

    let handler = CommandHandler::spawn_actor(
        Arc::clone(runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );

    ServerSystem {
        event_bus,
        handler,
        scheduler,
    }
}
