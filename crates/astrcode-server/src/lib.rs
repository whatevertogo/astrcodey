//! astrcode-server: Backend server runtime.

pub mod acp;
pub mod bootstrap;
pub mod http;
pub mod task_utils;
pub mod transport;

#[cfg(any(test, feature = "testing"))]
pub mod test_support;

mod child_session;
mod config_manager;
mod handler;
mod presentation;
mod server_event_bus;
mod session_command_service;
mod session_manager;
mod session_operations;
mod session_resource_cleanup;
mod turn_registry;
mod turn_scheduler;
