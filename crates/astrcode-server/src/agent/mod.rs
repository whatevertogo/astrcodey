//! Agent 模块 — 回合处理器与相关工具。

pub(crate) mod compact;
mod r#loop;
pub(crate) mod post_compact;

pub use compact::AutoCompactFailureTracker;
pub use r#loop::{AgentCompactContinuation, AgentError, AgentLoop, AgentServices, AgentTurnOutput};
pub(crate) use r#loop::{AgentSignal, drive_agent, tool_name_matches_allowlist};
