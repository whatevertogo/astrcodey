//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod agent;
/// Backward-compat alias so existing `use crate::agent_loop::*` paths keep working.
pub mod agent_loop {
    pub use crate::agent::{Agent, AgentError, AgentServices, AgentTurnOutput};
}
pub mod bootstrap;
pub mod handler;
pub mod http;
pub mod session;
pub mod transport;
