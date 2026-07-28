//! astrcode-session：会话运行时。
//!
//! 负责 Session 生命周期、Turn 执行、工具管线、事件发射和 compact。

pub mod compact;
pub(crate) mod compact_circuit_breaker;
pub(crate) mod compaction_coordinator;
pub mod compaction_run;
pub(crate) mod deferred_tools;
pub(crate) mod early_tool_scheduler;
pub(crate) mod llm_request_history;
pub(crate) mod llm_stream;
pub mod payload;
pub(crate) mod permission;
pub(crate) mod runtime_stability;
mod session;
mod session_extension_ports;
mod session_runtime;
mod session_runtime_services;
pub(crate) mod session_setup;
pub(crate) mod session_tools;
pub(crate) mod steer;
pub(crate) mod tool_deduplicator;
pub(crate) mod tool_exec;
pub(crate) mod tool_json_repair;
pub(crate) mod tool_pipeline;
mod tool_registry;
pub(crate) mod tool_results;
pub(crate) mod tool_types;
mod turn_context;
mod turn_handle;
pub(crate) mod turn_publish;
pub(crate) mod turn_runner;
pub(crate) mod turn_stages;

pub use payload::{
    agent_session_completed_payload, agent_session_failed_payload, compact_boundary_payload,
    session_continued_from_compaction_payload, system_prompt_configured_payload,
};
pub use session::{
    InterruptedToolOutcome, Session, SessionCreateParams, SessionError,
    emit_interrupted_tool_results, emit_lifecycle_for_read_model, emit_turn_aborted_context,
};
pub use session_extension_ports::SessionExtensionPorts;
pub use session_runtime::{
    SessionModelBinding, SessionRuntimeState, ToolApprovalResolveError, ToolUiResponseResolveError,
};
pub use session_runtime_services::{SessionHostServices, SessionRuntimeServices};
pub use tool_registry::ToolRegistry;
pub use turn_context::{TurnError, TurnEventTx};
pub use turn_handle::{TurnHandle, TurnShutdownHandle};
pub use turn_runner::{RunTurnResult, TurnOutput};
