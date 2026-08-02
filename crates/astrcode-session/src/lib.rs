//! astrcode-session：会话运行时。
//!
//! 负责 Session 生命周期、Turn 执行、工具管线、事件发射和 compact。

pub mod compaction;
pub(crate) mod deferred_tools;
pub(crate) mod early_tool_scheduler;
pub(crate) mod llm_stream;
pub mod payload;
mod perf_snapshot;
pub(crate) mod permission;
pub(crate) mod projection_context;
pub(crate) mod runtime_stability;
mod session;
mod session_compaction;
mod session_error;
mod session_event_sink;
mod session_extension_ports;
mod session_lifecycle;
mod session_prompt;
mod session_resource_store;
mod session_runtime;
mod session_runtime_services;
pub(crate) mod session_setup;
mod session_state;
pub(crate) mod session_tools;
mod session_turn;
pub(crate) mod steer;
#[cfg(test)]
pub(crate) mod test_support;
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
    agent_session_completed_payload, agent_session_failed_payload,
    system_prompt_configured_payload, transcript_rewritten_payload,
};
pub use session::{Session, SessionCreateParams, emit_lifecycle_for_read_model};
pub use session_error::SessionError;
pub use session_event_sink::{
    SessionEventObserver, SessionEventPublicationGuard, SessionEventPublishError, SessionEventSink,
};
pub use session_extension_ports::SessionExtensionPorts;
pub use session_lifecycle::SpawnChildParams;
pub use session_resource_store::SessionResourceStore;
pub use session_runtime::{
    SessionCreationFailed, SessionCreationGuard, SessionRuntimeState,
    ToolApprovalRegistrationError, ToolApprovalResolveError,
};
pub use session_runtime_services::SessionRuntimeServices;
pub use session_turn::{
    InterruptedToolOutcome, emit_interrupted_tool_results, emit_turn_aborted_context,
    finalize_aborted_turn, finalize_turn,
};
pub use tool_registry::{ToolRegistry, ToolRegistryError};
pub use turn_context::{TurnError, TurnEventTx};
pub use turn_handle::{TurnHandle, TurnShutdownHandle};
pub use turn_runner::{RunTurnResult, TurnFinalization, TurnOutput};
