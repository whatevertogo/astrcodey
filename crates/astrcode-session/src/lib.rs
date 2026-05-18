//! astrcode-session：会话运行时。
//!
//! 负责 Session 生命周期、Turn 执行、工具管线、事件发射和 compact。

pub mod background;
pub mod capabilities;
pub mod compact;
pub(crate) mod json_repair;
pub(crate) mod llm_stream;
pub(crate) mod mcp_visibility;
pub mod payload;
pub mod post_compact;
pub mod session;
pub mod session_runtime;
pub mod session_setup;
pub(crate) mod tool_exec;
pub(crate) mod tool_pipeline;
pub(crate) mod tool_types;
pub mod turn_context;
pub mod turn_handle;
pub(crate) mod turn_runner;


pub use background::{BackgroundTaskManager, spawn_background_forwarder};
pub use capabilities::Capabilities;
pub use payload::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    compact_boundary_payload, session_continued_from_compaction_payload,
};
pub use session::{Session, SessionError};
pub use session_runtime::SessionRuntimeState;
pub use turn_context::{AgentSignal, EventSink, TurnError};
pub use turn_handle::TurnHandle;
pub use turn_runner::{RunTurnResult, TurnOutput, TurnRunner, drive_agent, run_turn};
