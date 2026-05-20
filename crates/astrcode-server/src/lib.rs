//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod acp;
pub mod bootstrap;
pub mod handler;
pub mod http;
pub mod transport;

pub mod config_manager;
pub mod server_event_bus;
pub mod session_manager;
pub mod session_operations;
