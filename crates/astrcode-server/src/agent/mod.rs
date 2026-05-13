//! Agent 模块 — 回合处理器与相关工具。

pub mod background;
pub mod collaboration;
pub(crate) mod compact;
mod r#loop;
pub(crate) mod post_compact;
pub(crate) mod shared_context;
pub(crate) mod tool_exec;
pub(super) mod tool_pipeline;
pub(crate) mod tool_types;
pub(crate) mod util;

pub use background::BackgroundTaskManager;
pub use compact::AutoCompactFailureTracker;
pub(crate) use r#loop::drive_agent;
pub use r#loop::{AgentCompactContinuation, AgentLoop, AgentServices, AgentTurnOutput};
pub use shared_context::AgentError;
pub(crate) use shared_context::AgentSignal;
