//! astrcode-storage: Session persistence and config storage.
//!
//! JSONL event log, snapshots, file locking, and atomic config writes.

pub mod config_store;
pub mod event_log;
#[cfg(feature = "testing")]
pub mod in_memory;
pub mod lock;
mod projection;
pub mod session_repo;
pub mod snapshot;
