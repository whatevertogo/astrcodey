//! Turn 子系统：runner、driver、event bus、payload helpers。

pub(crate) mod context;
pub(crate) mod llm_stream;
pub(crate) mod payloads;
pub(crate) mod runner;

pub use context::{AgentSignal, EventBus, NoopEventBus, TurnError};
pub use payloads::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
};
pub use runner::{RunTurnResult, TurnOutput, TurnRunner, drive_agent, run_turn};
