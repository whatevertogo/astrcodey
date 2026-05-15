//! astrcode-session：会话运行时。
//!
//! 负责管理 Session 的生命周期和 turn 执行：
//! - `session`：Session 句柄，事件持久化和投影重建的入口
//! - `payload`：turn 生命周期事件构造
//! - `event_bus`：事件总线（后续 TurnRunner 的发射目标）

pub mod event_bus;
pub mod payload;
pub mod session;

pub use event_bus::{EventBus, NoopEventBus};
pub use payload::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    compact_boundary_payload, session_continued_from_compaction_payload,
};
pub use session::{
    SameSessionCompactionInput, Session, SessionError, append_same_session_compaction,
};
