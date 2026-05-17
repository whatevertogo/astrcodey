//! astrcode-session：会话运行时。
//!
//! Session 是系统唯一的持久事实来源，本 crate 提供：
//!
//! - `Session`：带存储能力的会话句柄（durable write 入口）
//! - `TurnRunner` / `run_turn`：临时 turn 处理器与驱动
//! - `SessionServices`：turn 执行所需的依赖容器
//! - `EventBus`：turn 事件回投契约
//! - `compact::*`：compact pipeline 与 hook 桥接
//! - `background::*`：后台任务管理
//!
//! 上层 actor 化和编排由 `astrcode-server` 负责。

pub mod background;
pub mod compact;
pub mod session;
pub mod turn;

pub(crate) mod json_repair;
pub(crate) mod runtime;
pub(crate) mod services;
pub(crate) mod tool;

pub use compact::{compact_boundary_payload, session_continued_from_compaction_payload};
pub use runtime::{SessionRuntimeRegistry, SessionRuntimeState};
pub use services::SessionServices;
pub use session::{Session, SessionError};
pub use turn::{
    AgentSignal, EventBus, NoopEventBus, RunTurnResult, TurnError, TurnOutput, TurnRunner,
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    drive_agent, run_turn,
};
