//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, HTTP/SSE API, ACP WebSocket, config service.

pub mod acp;
pub mod bootstrap;
pub mod handler;
pub mod http;

pub mod config_manager;
pub mod server_event_bus;
pub mod session_manager;
pub mod session_operations;
pub mod turn_registry;
pub mod turn_scheduler;

#[cfg(feature = "testing")]
pub mod testing;
