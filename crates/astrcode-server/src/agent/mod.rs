//! Agent 模块 — 回合处理器与相关工具。

pub(crate) mod compact;
mod r#loop;

pub use r#loop::{Agent, AgentCompactContinuation, AgentError, AgentServices, AgentTurnOutput};
pub(crate) use r#loop::{AgentSignal, drive_agent, tool_name_matches_allowlist};
