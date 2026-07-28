//! astrcode-storage: Session persistence and config storage.
//!
//! JSONL event log, snapshots, and atomic config writes.

pub mod config_store;
pub mod event_log;
#[cfg(feature = "testing")]
pub mod in_memory;
pub mod projection;
pub mod session_repo;
pub(crate) mod snapshot;
pub(crate) mod tool_artifacts;
