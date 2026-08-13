//! Pure session state derived from durable core events.
//!
//! This crate owns read models and reducers. It intentionally contains no
//! storage I/O or session orchestration.

mod agents;
mod error;
mod execution;
mod model;
mod model_context;
mod presentation;
mod reducer;

pub use agents::{AgentSessionLinkView, AgentSessionStatus};
pub use error::ProjectionError;
pub use execution::{ActiveStepView, PendingInput, PendingToolApprovalView, SessionExecutionState};
pub use model::{
    ForkSourceRef, SessionEventStats, SessionIdentity, SessionReadModel, SessionSummary,
};
pub use model_context::{
    CompactionView, ContextUsageView, SequencedLlmMessage, SessionModelContext,
    SessionSystemPrompt, TOOL_CALL_CANCELLED_SOURCE, TOOL_CALL_FAILED_SOURCE, UnansweredToolCall,
};
pub use presentation::{SessionArtifactView, SessionPresentation};
pub use reducer::{
    PreparedProjectionBatch, SessionReadModelProjection, SessionSummaryProjection, reduce, replay,
};
