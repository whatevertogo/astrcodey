//! astrcode-server: Backend server runtime.

pub mod acp;
pub mod bootstrap;
pub mod http;
pub mod task_utils;
pub mod transport;

pub use child_session::{ChildCleanup, ChildSessionCoordinator};
pub use config_manager::ConfigManager;
pub use handler::CommandHandle;
pub use server_event_bus::ServerEventBus;
pub use session_command_contract::{
    CommandInvocation, CommandList, HandlerError, ManualCompactOutcome, PromptSubmission,
};
pub use session_manager::{SessionManager, SessionManagerError};
pub use turn_registry::TurnRegistry;
pub use turn_scheduler::{
    DeliveryOutcome, InputDelivery, MAX_PENDING_INPUTS_PER_SESSION, MAX_PROMPT_TEXT_BYTES,
    SessionExecutionView, StartedExecution, TurnScheduleError, TurnScheduler,
};

#[cfg(any(test, feature = "testing"))]
pub mod test_support;

mod child_session;
mod config_manager;
mod delivery_gates;
mod handler;
mod presentation;
mod protocol_mapping;
mod queue_drains;
mod server_event_bus;
mod session_command_contract;
mod session_command_service;
mod session_manager;
mod session_operations;
mod session_resource_cleanup;
mod turn_registry;
mod turn_scheduler;
