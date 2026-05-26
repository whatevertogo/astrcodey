//! Handler 与 TurnScheduler 之间的错误映射。

use super::HandlerError;
use crate::turn_scheduler::TurnScheduleError;

impl From<TurnScheduleError> for HandlerError {
    fn from(error: TurnScheduleError) -> Self {
        match error {
            TurnScheduleError::TurnAlreadyRunning => HandlerError::TurnAlreadyRunning,
            TurnScheduleError::NoActiveTurn => HandlerError::NoActiveTurn,
            TurnScheduleError::SessionNotFound(msg) => HandlerError::SessionNotFound(msg),
            TurnScheduleError::SessionManager(e) => HandlerError::SessionManager(e),
            TurnScheduleError::Session(e) => HandlerError::Session(e),
            TurnScheduleError::Turn(e) => HandlerError::Turn(e),
            TurnScheduleError::EventEmit(e) => HandlerError::Session(e),
        }
    }
}

/// 将 turn 调度错误映射为客户端错误码与 handler 错误（用于需要 `send_error` 的路径）。
pub(crate) fn turn_schedule_error_for_client(error: TurnScheduleError) -> (i32, HandlerError) {
    match &error {
        TurnScheduleError::TurnAlreadyRunning => (40900, HandlerError::TurnAlreadyRunning),
        _ => (-32603, HandlerError::from(error)),
    }
}
