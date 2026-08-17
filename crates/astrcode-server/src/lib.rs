//! astrcode-server: Backend server runtime.

pub mod acp;
pub mod bootstrap;
pub mod http;
mod task_utils;
pub mod transport;

pub use handler::CommandHandle;
pub use server_event_bus::ServerEventBus;
pub use session_command_contract::HandlerError;
pub use session_manager::SessionManagerError;

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
