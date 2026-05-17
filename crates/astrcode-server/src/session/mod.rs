//! Session 子系统：directory + bootstrapper + per-session actor + supervisor + slash/compact/turn
//! 行为。
//!
//! Session 是系统唯一的持久事实来源。任何会改变 session durable state 的写入
//! 都通过 `SessionActor` 串行执行；外部（router、coordinator）通过
//! `SessionSupervisor` 拿到 `SessionHandle` 投递命令。

mod actor;
pub(crate) mod bootstrapper;
mod compact;
pub(crate) mod directory;
pub(crate) mod slash;
pub(crate) mod snapshot;
mod supervisor;
mod turn;

pub use actor::{SessionActor, SessionHandle};
pub use bootstrapper::SessionBootstrapper;
pub use compact::ManualCompactOutcome;
pub use directory::{SessionDirectory, SessionDirectoryError};
#[cfg(test)]
pub(crate) use snapshot::message_to_dto;
pub(crate) use snapshot::session_snapshot;
pub use supervisor::{BoundSessionMessenger, SessionSupervisor};
pub(crate) use turn::TurnCompletion;
