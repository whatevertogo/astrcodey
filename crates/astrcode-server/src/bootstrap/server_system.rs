//! Server 核心系统组装 — 事件总线 + scheduler + handler actor。

use std::sync::Arc;

use astrcode_protocol::events::ClientNotification;
use astrcode_support::event_fanout::EventFanout;

use super::ServerRuntime;
use crate::{
    handler::CommandHandle, server_event_bus::ServerEventBus, turn_scheduler::TurnScheduler,
};

pub struct ServerSystem {
    pub event_tx: Arc<EventFanout<ClientNotification>>,
    pub event_bus: Arc<ServerEventBus>,
    pub handler: CommandHandle,
    pub scheduler: Arc<TurnScheduler>,
}

pub fn spawn_server_system(
    runtime: &Arc<ServerRuntime>,
    event_tx: Arc<EventFanout<ClientNotification>>,
) -> ServerSystem {
    let scheduler = Arc::clone(runtime.scheduler());

    let event_bus = Arc::new(ServerEventBus::new(Arc::clone(&event_tx)));

    runtime
        .session_manager()
        .bind_event_bus(Arc::clone(&event_bus));

    let handler = CommandHandle::spawn(
        Arc::clone(runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );

    ServerSystem {
        event_tx,
        event_bus,
        handler,
        scheduler,
    }
}
