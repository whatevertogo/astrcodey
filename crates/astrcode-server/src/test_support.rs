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
