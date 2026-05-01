//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod agent_loop;
pub mod agent {
    pub use crate::agent_loop::{Agent, AgentServices, AgentTurnOutput};
}
pub mod bootstrap;
pub(crate) mod forked_provider;
pub mod handler;
pub mod session;
pub(crate) mod session_spawner;
pub mod transport;
