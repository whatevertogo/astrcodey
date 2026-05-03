//! astrcode-server: Backend server runtime.
//!
//! Session manager, agent loop, JSON-RPC transport handler,
//! config service, and multi-session concurrency.

pub mod agent;
pub mod bootstrap;
pub mod handler;
pub mod http;
pub mod session;
pub mod transport;
