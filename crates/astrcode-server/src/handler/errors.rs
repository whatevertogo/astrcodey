//! Handler 与 TurnScheduler 之间的错误映射。

use super::HandlerError;
use crate::turn_scheduler::TurnScheduleError;

impl From<TurnScheduleError> for HandlerError {
    fn from(error: TurnScheduleError) -> Self {
        match error {
            TurnScheduleError::TurnAlreadyRunning => Self::TurnAlreadyRunning,
            TurnScheduleError::NoActiveTurn => Self::NoActiveTurn,
            TurnScheduleError::QueueFull { .. }
            | TurnScheduleError::EmptyInput
            | TurnScheduleError::InputTooLarge { .. } => Self::InvalidRequest(error.to_string()),
            TurnScheduleError::SessionNotFound(message) => Self::SessionNotFound(message),
            TurnScheduleError::SessionManager(error) => Self::SessionManager(error),
            TurnScheduleError::Session(error) | TurnScheduleError::EventEmit(error) => {
                Self::Session(error)
            },
            TurnScheduleError::Turn(error) => Self::Turn(error),
            error @ (TurnScheduleError::RecycleRelationRollbackFailed { .. }
            | TurnScheduleError::DeleteRelationUpdateFailed { .. }
            | TurnScheduleError::ChildRelationConflict { .. }
            | TurnScheduleError::CompletionOwnershipLost { .. }) => {
                Self::SessionClose(error.to_string())
            },
        }
    }
}
