//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod acp;
pub mod agent;
pub mod bootstrap;
pub mod handler;
pub mod http;
pub mod transport;

pub(crate) mod server_event_bus;
pub(crate) mod session_spawner;
