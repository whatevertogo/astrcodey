use std::collections::HashMap;

use astrcode_core::{
    event::{Event, EventPayload, Phase},
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
        match &event.payload {
            EventPayload::AgentSessionSpawned {
                child_session_id, ..
            } => {
                self.initial_by_leaf
                    .insert(child_session_id.clone(), child_session_id.clone());
                self.last_progress
                    .insert(child_session_id.clone(), (Phase::Thinking, None));
            },
            EventPayload::AgentSessionCompleted {
                child_session_id, ..
            }
            | EventPayload::AgentSessionFailed {
                child_session_id, ..
            }
            | EventPayload::AgentSessionRecycled { child_session_id } => {
                self.initial_by_leaf
                    .retain(|_, initial| initial != child_session_id);
                self.last_progress.remove(child_session_id);
            },
            _ => {},
        }
    }

    pub(super) fn is_tracked_event(&self, event: &Event) -> bool {
        if self.initial_by_leaf.contains_key(&event.session_id) {
            return true;
        }
        matches!(
            &event.payload,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id,
                ..
            } if self.initial_by_leaf.contains_key(parent_session_id)
        )
    }

    pub(super) fn project_event(&mut self, event: &Event) -> Option<ConversationDeltaDto> {
        if self.update_compacted_leaf(event) {
            return None;
        }

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

    fn update_compacted_leaf(&mut self, event: &Event) -> bool {
        let EventPayload::SessionContinuedFromCompaction {
            parent_session_id, ..
        } = &event.payload
        else {
            return false;
        };
        let Some(initial_child_id) = self.initial_by_leaf.remove(parent_session_id) else {
            return true;
        };
        self.initial_by_leaf
            .insert(event.session_id.clone(), initial_child_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_phase_deduplication_and_compacted_leaf_replacement() {
        let initial = SessionId::from("child-initial");
        let compacted = SessionId::from("child-compacted");
        let mut tracker = ChildSessionTracker::new(
            HashMap::from([(initial.clone(), initial.clone())]),
            HashMap::from([(initial.clone(), Phase::Thinking)]),
        );

        let duplicate = Event::new(initial.clone(), None, EventPayload::TurnStarted);
        assert!(tracker.project_event(&duplicate).is_none());

        let tool = Event::new(
            initial.clone(),
            None,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "read".into(),
            },
        );
        assert!(matches!(
            tracker.project_event(&tool),
            Some(ConversationDeltaDto::AgentSessionUpdated { .. })
        ));

        let repeated_tool = Event::new(
            initial.clone(),
            None,
            EventPayload::ToolCallRequested {
                call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
            },
        );
        assert!(tracker.project_event(&repeated_tool).is_none());

        let text_delta = Event::new(
            initial.clone(),
            None,
            EventPayload::AssistantTextDelta {
                message_id: "message-1".into(),
                delta: "token".into(),
            },
        );
        assert!(tracker.project_event(&text_delta).is_none());

        let continued = Event::new(
            compacted.clone(),
            None,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: initial.clone(),
                parent_cursor: "4".into(),
                summary: "summary".into(),
                transcript_path: None,
                context_messages: Vec::new(),
                retained_messages: Vec::new(),
            },
        );
        assert!(tracker.is_tracked_event(&continued));
        assert!(tracker.project_event(&continued).is_none());

        let streaming = Event::new(
            compacted,
            None,
            EventPayload::AssistantMessageStarted {
                message_id: "message-1".into(),
            },
        );
        assert!(matches!(
            tracker.project_event(&streaming),
            Some(ConversationDeltaDto::AgentSessionUpdated { .. })
        ));
    }
}
