//! Agent 模块 — 回合处理器与相关工具。
//!
//! 对外 re-export 主要类型，保持 `crate::agent_loop::*` 兼容路径。

pub(crate) mod compact;
mod r#loop;

pub use r#loop::{Agent, AgentError, AgentServices, AgentTurnOutput};
pub(crate) use r#loop::{drive_agent, tool_name_matches_allowlist};
