//! astrcode-session：会话运行时。
//!
//! 负责 Session 生命周期、Turn 执行、工具管线、事件发射和 compact。

pub mod background;
pub mod compact;
pub mod event_bus;
pub mod payload;
pub mod post_compact;
pub mod session;
pub mod session_context;
pub mod tool_exec;
pub mod tool_pipeline;
pub mod tool_types;
pub mod turn_context;
pub mod turn_runner;
pub mod util;

pub use compact::AutoCompactFailureTracker;
pub use event_bus::{EventBus, NoopEventBus};
pub use payload::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    compact_boundary_payload, session_continued_from_compaction_payload,
};
pub use session::{Session, SessionError};
pub use session_context::SessionContext;
pub use turn_context::{AgentError, AgentSignal};
pub use turn_runner::{CompactContinuation, TurnOutput, TurnRunner, drive_agent};
