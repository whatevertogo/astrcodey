use std::collections::{HashMap, HashSet};

use astrcode_core::{
    event::{DurableEventPayload, Event, EventPayload, LiveEventPayload},
    types::{Cursor, MessageId, SessionId, ToolCallId, TurnId},
};

use crate::{SessionManagerError, bootstrap::ServerRuntime};

const MAX_REPLAY_EVENTS: usize = 1_000;

#[derive(Default)]
pub(super) struct ReplayedSettlements {
    messages: HashSet<MessageId>,
    calls: HashMap<Option<TurnId>, HashSet<ToolCallId>>,
    turns: HashSet<TurnId>,
}

impl ReplayedSettlements {
    pub(super) fn new(events: &[Event]) -> Self {
        let mut settled = Self::default();
        for event in events {
            match event.payload.as_durable() {
                Some(
                    DurableEventPayload::AssistantMessage { message_id, .. }
                    | DurableEventPayload::AssistantAttemptSettled { message_id, .. },
                ) => {
                    settled.messages.insert(message_id.clone());
                },
                Some(DurableEventPayload::ToolCallSettled { call_id, .. }) => {
                    settled
                        .calls
                        .entry(event.turn_id.clone())
                        .or_default()
                        .insert(call_id.clone());
                },
                Some(DurableEventPayload::TurnEnded { .. }) => {
                    if let Some(turn_id) = &event.turn_id {
                        settled.turns.insert(turn_id.clone());
                    }
                },
                _ => {},
            }
        }
        settled
    }

    pub(super) fn contains(&self, event: &Event) -> bool {
        let EventPayload::Live(payload) = &event.payload else {
            return false;
        };
        let ended = event
            .turn_id
            .as_ref()
            .is_some_and(|id| self.turns.contains(id));
        match payload {
            LiveEventPayload::AssistantMessageStarted { message_id }
            | LiveEventPayload::AssistantMessageDelta { message_id, .. } => {
                ended || self.messages.contains(message_id)
            },
            LiveEventPayload::ToolCallPreviewStarted {
                message_id,
                call_id,
                ..
            }
            | LiveEventPayload::ToolCallArgumentsDelta {
                message_id,
                call_id,
                ..
            } => {
                ended
                    || self.messages.contains(message_id)
                    || self
                        .calls
                        .get(&event.turn_id)
                        .is_some_and(|calls| calls.contains(call_id))
            },
            LiveEventPayload::ToolExecutionStarted { call_id, .. }
            | LiveEventPayload::ToolCallPreviewDiscarded { call_id }
            | LiveEventPayload::ToolOutputDelta { call_id, .. } => {
                ended
                    || self
                        .calls
                        .get(&event.turn_id)
                        .is_some_and(|calls| calls.contains(call_id))
            },
            LiveEventPayload::CustomEvent(_) => false,
            _ => ended,
        }
    }
}

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
}

pub(super) fn parse_replay_cursor(cursor: &str) -> Result<u64, ReplayError> {
    cursor.parse().map_err(|_| ReplayError::InvalidCursor {
        cursor: cursor.into(),
    })
}

async fn latest_event_seq(
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

#[cfg(test)]
mod tests {
    use astrcode_core::event::{AssistantAttemptOutcome, DurableEvent, LiveEvent, StoredEvent};

    use super::*;

    #[test]
    fn replay_filters_settled_identities_and_preserves_unfinished_output() {
        let durable = |turn: &str, payload| {
            Event::from(StoredEvent::new(
                1,
                DurableEvent::turn("session".into(), turn.into(), payload),
            ))
        };
        let settled = ReplayedSettlements::new(&[
            durable(
                "old-turn",
                DurableEventPayload::ToolCallSettled {
                    call_id: "reused".into(),
                    outcome: astrcode_core::event::ToolCallOutcome::Completed,
                    result: astrcode_core::tool::ToolResult::success("done"),
                },
            ),
            durable(
                "new-turn",
                DurableEventPayload::AssistantAttemptSettled {
                    message_id: "failed-attempt".into(),
                    outcome: AssistantAttemptOutcome::Cancelled,
                },
            ),
        ]);
        for (turn, message, call, expected) in [
            ("old-turn", "old-message", "reused", true),
            ("new-turn", "active-attempt", "reused", false),
            ("new-turn", "failed-attempt", "preview-only", true),
        ] {
            let preview = Event::from(LiveEvent::turn(
                "session".into(),
                turn.into(),
                LiveEventPayload::ToolCallPreviewStarted {
                    message_id: message.into(),
                    call_id: call.into(),
                    tool_name: "read".into(),
                },
            ));
            let delta = Event::from(LiveEvent::turn(
                "session".into(),
                turn.into(),
                LiveEventPayload::ToolCallArgumentsDelta {
                    message_id: message.into(),
                    call_id: call.into(),
                    delta: "{}".into(),
                },
            ));
            assert_eq!(settled.contains(&preview), expected);
            assert_eq!(settled.contains(&delta), expected);
        }
    }
}
