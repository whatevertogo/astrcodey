//! astrcode-session：会话运行时。
//!
//! 负责 Session 生命周期、Turn 执行、工具管线、事件发射和 compact。

pub mod background;
pub mod child_turn;
pub mod compact;
pub(crate) mod compact_circuit_breaker;
pub(crate) mod deferred_tools;
pub(crate) mod llm_stream;
pub mod payload;
pub mod post_compact;
pub mod session;
pub mod session_runtime;
pub mod session_runtime_services;
pub mod session_setup;
pub(crate) mod tool_exec;
pub(crate) mod tool_json_repair;
pub(crate) mod tool_pipeline;
pub(crate) mod tool_results;
pub(crate) mod tool_types;
pub mod turn_context;
pub mod turn_handle;
pub(crate) mod turn_runner;
pub(crate) mod turn_stages;

pub use background::{BackgroundTaskManager, spawn_background_forwarder};
pub use payload::{compact_boundary_payload, session_continued_from_compaction_payload};
pub use session::{Session, SessionError};
pub use session_runtime::SessionRuntimeState;
pub use session_runtime_services::SessionRuntimeServices;
pub use turn_context::{AgentSignal, TurnError};
pub use turn_handle::TurnHandle;
pub use turn_runner::{RunTurnResult, TurnOutput, TurnRunner, drive_agent, run_turn};
