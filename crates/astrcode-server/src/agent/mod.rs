//! Agent 模块 — re-export 自 `astrcode_session`.

pub mod collaboration;

pub use astrcode_session::{
    SessionServices,
    background::BackgroundTaskManager,
    compact::AutoCompactFailureTracker,
    turn_context::{TurnError, AgentSignal},
    turn_runner::{CompactContinuation, TurnOutput, TurnRunner, drive_agent},
};
