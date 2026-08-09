//! astrcode-storage: Session persistence and config storage.
//!
//! JSONL event log, snapshots, and atomic config writes.

pub mod config_store;
mod error;
pub mod event_log;
#[cfg(feature = "testing")]
pub mod in_memory;
pub mod session_repo;
pub(crate) mod snapshot;
pub(crate) mod tool_artifacts;
mod traits;
mod types;

#[cfg(test)]
mod test_support;

pub use astrcode_core::tool::ToolResultArtifactSlice;
pub use error::StorageError;
pub use traits::{
    EventConsumerCheckpointOutcome, EventConsumerCheckpointReset, EventConsumerState, EventReader,
    SessionEventJournal, SessionPathResolver, SessionReader, SessionStore, ToolResultArtifactStore,
};
pub use types::{CompactSnapshotInput, ToolResultArtifactInput, ToolResultArtifactRef};
