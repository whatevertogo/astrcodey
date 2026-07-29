//! Handler 与 TurnScheduler 之间的错误映射。

use super::HandlerError;
use crate::turn_scheduler::TurnScheduleError;

impl From<TurnScheduleError> for HandlerError {
    fn from(error: TurnScheduleError) -> Self {
        match error {
            TurnScheduleError::TurnAlreadyRunning => HandlerError::TurnAlreadyRunning,
            TurnScheduleError::NoActiveTurn => HandlerError::NoActiveTurn,
            TurnScheduleError::QueueFull { .. }
            | TurnScheduleError::EmptyInput
            | TurnScheduleError::InputTooLarge { .. } => {
                HandlerError::InvalidRequest(error.to_string())
            },
            TurnScheduleError::SessionNotFound(msg) => HandlerError::SessionNotFound(msg),
            TurnScheduleError::SessionManager(e) => HandlerError::SessionManager(e),
            TurnScheduleError::Session(e) => HandlerError::Session(e),
            TurnScheduleError::Turn(e) => HandlerError::Turn(e),
            TurnScheduleError::EventEmit(e) => HandlerError::Session(e),
        }
    }
}
