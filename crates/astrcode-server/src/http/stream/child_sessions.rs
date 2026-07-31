use std::collections::HashMap;

use astrcode_core::{
    event::{DurableEventPayload, Event, Phase},
    types::SessionId,
};
use astrcode_protocol::{agent_session_link::AgentSessionLinkDto, http::ConversationDeltaDto};

use crate::server_event_bus::agent_session_progress;

/// Tracks the initial child session shown by the parent and its current compacted leaf.
pub(super) struct ChildSessionTracker {
    initial_by_leaf: HashMap<SessionId, SessionId>,
    last_progress: HashMap<SessionId, (Phase, Option<String>)>,
}

impl ChildSessionTracker {
    pub(super) fn new(
        leaf_by_initial: HashMap<SessionId, SessionId>,
        last_phase: HashMap<SessionId, Phase>,
    ) -> Self {
        let initial_by_leaf = leaf_by_initial
            .iter()
            .map(|(initial, leaf)| (leaf.clone(), initial.clone()))
            .collect();
        Self {
            initial_by_leaf,
            last_progress: last_phase
                .into_iter()
                .map(|(session_id, phase)| (session_id, (phase, None)))
                .collect(),
        }
    }

    pub(super) fn update_from_parent_event(&mut self, event: &Event) {
        let Some(payload) = event.payload.as_durable() else {
            return;
        };
        match payload {
            DurableEventPayload::AgentSessionSpawned {
                child_session_id, ..
            } => {
                self.initial_by_leaf
                    .insert(child_session_id.clone(), child_session_id.clone());
                self.last_progress
                    .insert(child_session_id.clone(), (Phase::Thinking, None));
            },
            DurableEventPayload::AgentSessionCompleted {
                child_session_id, ..
            }
            | DurableEventPayload::AgentSessionFailed {
                child_session_id, ..
            }
            | DurableEventPayload::AgentSessionRecycled { child_session_id } => {
                self.initial_by_leaf
                    .retain(|_, initial| initial != child_session_id);
                self.last_progress.remove(child_session_id);
            },
            _ => {},
        }
    }

    pub(super) fn is_tracked_event(&self, event: &Event) -> bool {
        self.initial_by_leaf.contains_key(&event.session_id)
    }

    pub(super) fn project_event(&mut self, event: &Event) -> Option<ConversationDeltaDto> {
        let initial_child_id = self.initial_by_leaf.get(&event.session_id)?.clone();
        let progress = agent_session_progress(&event.payload)?;
        if self.last_progress.get(&initial_child_id) == Some(&progress) {
            return None;
        }
        self.last_progress
            .insert(initial_child_id.clone(), progress.clone());
        let (phase, current_tool) = progress;

        Some(ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::phase_only(
                initial_child_id,
                phase.into(),
                current_tool,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::{DurableEvent, LiveEvent, LiveEventPayload, StoredEvent};

    use super::*;

    fn durable(session_id: SessionId, payload: DurableEventPayload) -> Event {
        StoredEvent::new(1, DurableEvent::session(session_id, payload)).into()
    }

    fn live(session_id: SessionId, payload: LiveEventPayload) -> Event {
        LiveEvent::session(session_id, payload).into()
    }

    #[test]
    fn tracks_phase_and_deduplicates_progress() {
        let initial = SessionId::from("child-initial");
        let mut tracker = ChildSessionTracker::new(
            HashMap::from([(initial.clone(), initial.clone())]),
            HashMap::from([(initial.clone(), Phase::Thinking)]),
        );

        let duplicate = durable(initial.clone(), DurableEventPayload::TurnStarted);
        assert!(tracker.project_event(&duplicate).is_none());

        let tool = live(
            initial.clone(),
            LiveEventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "read".into(),
            },
        );
        assert!(matches!(
            tracker.project_event(&tool),
            Some(ConversationDeltaDto::AgentSessionUpdated { .. })
        ));

        let repeated_tool = durable(
            initial.clone(),
            DurableEventPayload::ToolCallRequested {
                call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
            },
        );
        assert!(tracker.project_event(&repeated_tool).is_none());

        let text_delta = live(
            initial.clone(),
            LiveEventPayload::AssistantTextDelta {
                message_id: "message-1".into(),
                delta: "token".into(),
            },
        );
        assert!(tracker.project_event(&text_delta).is_none());

        let streaming = live(
            initial,
            LiveEventPayload::AssistantMessageStarted {
                message_id: "message-1".into(),
            },
        );
        assert!(matches!(
            tracker.project_event(&streaming),
            Some(ConversationDeltaDto::AgentSessionUpdated { .. })
        ));
    }
}
