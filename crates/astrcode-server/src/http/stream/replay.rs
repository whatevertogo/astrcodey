use astrcode_core::{
    event::Event,
    types::{Cursor, SessionId},
};
use astrcode_storage::StorageError;

use crate::{SessionManagerError, bootstrap::ServerRuntime};

const MAX_REPLAY_EVENTS: usize = 1_000;

#[derive(Debug, thiserror::Error)]
pub(super) enum ReplayError {
    #[error("event cursor {cursor:?} is invalid")]
    InvalidCursor { cursor: String },
    #[error("event cursor {requested_seq} is ahead of the latest sequence {latest_seq}")]
    CursorAhead { requested_seq: u64, latest_seq: u64 },
    #[error("event replay exceeds the {limit}-event limit")]
    LimitExceeded { limit: usize },
    #[error("stored event cursor {cursor:?} is invalid: {source}")]
    InvalidStoredCursor {
        cursor: Cursor,
        source: std::num::ParseIntError,
    },
    #[error(transparent)]
    Session(#[from] SessionManagerError),
}

impl ReplayError {
    pub(super) fn is_cursor_unavailable(&self) -> bool {
        matches!(
            self,
            Self::InvalidCursor { .. } | Self::CursorAhead { .. } | Self::LimitExceeded { .. }
        )
    }

    pub(super) fn is_session_not_found(&self) -> bool {
        matches!(
            self,
            Self::Session(SessionManagerError::Storage(StorageError::NotFound(_)))
        )
    }
}

pub(super) fn parse_replay_cursor(cursor: &str) -> Result<u64, ReplayError> {
    cursor.parse().map_err(|_| ReplayError::InvalidCursor {
        cursor: cursor.into(),
    })
}

pub(super) async fn latest_event_seq(
    runtime: &ServerRuntime,
    session_id: &SessionId,
) -> Result<u64, ReplayError> {
    match runtime.session_manager.latest_cursor(session_id).await? {
        Some(cursor) => cursor
            .parse()
            .map_err(|source| ReplayError::InvalidStoredCursor { cursor, source }),
        None => Ok(0),
    }
}

pub(super) async fn replay_after_cursor(
    runtime: &ServerRuntime,
    session_id: &SessionId,
    requested_seq: u64,
) -> Result<Vec<Event>, ReplayError> {
    let latest_seq = latest_event_seq(runtime, session_id).await?;
    if requested_seq > latest_seq {
        return Err(ReplayError::CursorAhead {
            requested_seq,
            latest_seq,
        });
    }

    let cursor = requested_seq.to_string();
    let events = runtime
        .session_manager
        .replay_from_limited(session_id, &Cursor::from(cursor), MAX_REPLAY_EVENTS + 1)
        .await?;
    if events.len() > MAX_REPLAY_EVENTS {
        return Err(ReplayError::LimitExceeded {
            limit: MAX_REPLAY_EVENTS,
        });
    }
    Ok(events)
}
