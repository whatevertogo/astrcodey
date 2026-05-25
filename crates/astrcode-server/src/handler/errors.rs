//! Handler 与 TurnScheduler 之间的错误映射。

use super::HandlerError;
use crate::turn_scheduler::TurnError;

impl From<TurnError> for HandlerError {
    fn from(error: TurnError) -> Self {
        match error {
            TurnError::TurnAlreadyRunning => HandlerError::TurnAlreadyRunning,
            TurnError::NoActiveTurn => HandlerError::NoActiveTurn,
            TurnError::SessionNotFound(msg) => HandlerError::SessionNotFound(msg),
            TurnError::SessionManager(e) => HandlerError::SessionManager(e),
            TurnError::Session(e) => HandlerError::Session(e),
            TurnError::Turn(e) => HandlerError::Turn(e),
            TurnError::EventEmit(e) => HandlerError::Session(e),
        }
    }
}

/// 将 turn 错误映射为客户端错误码与 handler 错误（用于需要 `send_error` 的路径）。
pub(crate) fn turn_error_for_client(error: TurnError) -> (i32, HandlerError) {
    match &error {
        TurnError::TurnAlreadyRunning => (40900, HandlerError::TurnAlreadyRunning),
        _ => (-32603, HandlerError::from(error)),
    }
}
