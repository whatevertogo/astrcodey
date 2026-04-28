//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod agent;
mod agent_turn;
pub(crate) mod agent_types;
pub mod bootstrap;
pub mod handler;
pub mod session;
pub(crate) mod session_spawner;
pub mod transport;
