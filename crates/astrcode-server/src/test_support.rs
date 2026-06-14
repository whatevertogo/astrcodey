//! 集成测试构造内部编排组件（需 `testing` feature）。

pub use crate::{
    child_session::ChildSessionCoordinator,
    config_manager::ConfigManager,
    server_event_bus::ServerEventBus,
    session_manager::SessionManager,
    session_operations::ServerSessionOperations,
    turn_registry::TurnRegistry,
    turn_scheduler::{
        CompletionParams, DeliveryOutcome, InputDelivery, MAX_PENDING_INPUTS_PER_SESSION,
        MAX_PROMPT_TEXT_BYTES, StartedExecution, TurnScheduleError, TurnScheduler,
    },
};
