use astrcode_core::{
    event::Event,
    types::{Cursor, SessionId},
};

use crate::bootstrap::ServerRuntime;

const MAX_REPLAY_EVENTS: usize = 1_000;

pub(super) async fn replay_after_cursor(
    runtime: &ServerRuntime,
    session_id: &SessionId,
    cursor: &str,
) -> (Vec<Event>, bool) {
    let Ok(requested_seq) = cursor.parse::<u64>() else {
        return (Vec::new(), true);
    };
    let latest_seq = match runtime.session_manager.latest_cursor(session_id).await {
        Ok(Some(latest)) => match latest.parse::<u64>() {
            Ok(latest) => latest,
            Err(error) => {
                tracing::warn!(session_id = %session_id, latest, %error, "stored SSE cursor is invalid");
                return (Vec::new(), true);
            },
        },
        Ok(None) => 0,
        Err(error) => {
            tracing::warn!(session_id = %session_id, cursor, "failed to validate SSE cursor: {error}");
            return (Vec::new(), true);
        },
    };
    if requested_seq > latest_seq {
        tracing::info!(
            session_id = %session_id,
            cursor,
            latest_seq,
            "SSE cursor is ahead of the session; requesting rehydrate"
        );
        return (Vec::new(), true);
    }

    match runtime
        .session_manager
        .replay_from_limited(session_id, &Cursor::from(cursor), MAX_REPLAY_EVENTS + 1)
        .await
    {
        Ok(events) if events.len() > MAX_REPLAY_EVENTS => {
            tracing::info!(
                session_id = %session_id,
                cursor,
                replay_limit = MAX_REPLAY_EVENTS,
                "SSE replay limit exceeded; requesting rehydrate"
            );
            (Vec::new(), true)
        },
        Ok(events) => (events, false),
        Err(error) => {
            tracing::warn!(session_id = %session_id, cursor, "failed to replay SSE cursor: {error}");
            (Vec::new(), true)
        },
    }
}
