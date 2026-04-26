//! astrcode-protocol: Wire protocol types.
//!
//! JSON-RPC 2.0 message types: client commands, server events,
//! UI sub-protocol, session snapshots, and version negotiation.

pub mod commands;
pub mod events;
pub mod framing;
pub mod snapshot;
pub mod ui;
pub mod version;
