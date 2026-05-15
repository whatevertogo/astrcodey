//! Agent 模块 — re-export 自 `astrcode_session`.

pub mod collaboration;

pub use astrcode_session::{
    SessionContext,
    background::BackgroundTaskManager,
    compact::AutoCompactFailureTracker,
    turn_context::{AgentError, AgentSignal},
    turn_runner::{CompactContinuation, TurnOutput, TurnRunner, drive_agent},
};
