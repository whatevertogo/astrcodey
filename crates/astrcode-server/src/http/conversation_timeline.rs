//! Read-optimized conversation history boundary.
//!
//! HTTP depends on [`ConversationTimelineReader`], not on the event-log layout.
//! The current implementation derives bounded pages from durable events; a
//! materialized backend can replace it without changing the wire contract.

use std::{collections::HashSet, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event, StoredEvent},
    types::SessionId,
};
use astrcode_protocol::http::ConversationBlockDto;
use astrcode_storage::{SessionStore, StorageError};

use super::projection::blocks::{
    block_from_payload, persisted_transcript_blocks, streaming_tool_call_block,
};

const CURSOR_PREFIX: &str = "timeline-v1:";
const EVENT_CHUNK: usize = 512;
const EVENT_CHUNK_SCAN_TARGET: usize = 8;
pub(in crate::http) const DEFAULT_PAGE_ITEMS: usize = 50;
pub(in crate::http) const MAX_PAGE_ITEMS: usize = 100;
pub(in crate::http) const MAX_PAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::http) struct TimelineCursor {
    encoded: String,
    position: TimelinePosition,
}

impl TimelineCursor {
    pub(in crate::http) fn parse(value: String) -> Result<Self, ConversationTimelineError> {
        let position = decode_cursor(&value)?;
        Ok(Self {
            encoded: value,
            position,
        })
    }

    pub(in crate::http) fn into_string(self) -> String {
        self.encoded
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::http) struct PageBudget {
    pub(in crate::http) max_items: usize,
    pub(in crate::http) max_bytes: usize,
}

pub(in crate::http) struct ConversationTimelinePage {
    pub(in crate::http) items: Vec<ConversationBlockDto>,
    pub(in crate::http) older_cursor: Option<TimelineCursor>,
    pub(in crate::http) has_older: bool,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::http) enum ConversationTimelineError {
    #[error("invalid conversation timeline cursor")]
    InvalidCursor,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("failed to encode conversation item: {0}")]
    Encode(#[from] serde_json::Error),
}

#[async_trait::async_trait]
pub(in crate::http) trait ConversationTimelineReader: Send + Sync {
    async fn page_before(
        &self,
        session_id: &SessionId,
        before: Option<&TimelineCursor>,
        budget: PageBudget,
    ) -> Result<ConversationTimelinePage, ConversationTimelineError>;
}

pub(in crate::http) struct EventLogConversationTimeline {
    events: Arc<dyn SessionStore>,
}

impl EventLogConversationTimeline {
    pub(in crate::http) fn new(events: Arc<dyn SessionStore>) -> Self {
        Self { events }
    }
}

#[async_trait::async_trait]
impl ConversationTimelineReader for EventLogConversationTimeline {
    async fn page_before(
        &self,
        session_id: &SessionId,
        before: Option<&TimelineCursor>,
        budget: PageBudget,
    ) -> Result<ConversationTimelinePage, ConversationTimelineError> {
        if let Some(TimelineCursor {
            position:
                TimelinePosition::ForkItem {
                    event_seq,
                    item_index,
                },
            ..
        }) = before
        {
            return self
                .fork_page_before(session_id, *event_seq, *item_index, budget)
                .await;
        }

        let mut before_seq = before.map(|cursor| cursor.position.event_seq());
        let mut entries = Vec::new();
        let mut earliest_scanned_seq = None;
        let mut scanned_chunks = 0usize;

        let has_unscanned_events = loop {
            scanned_chunks += 1;
            let before_storage_cursor = before_seq.map(|seq| seq.to_string());
            let mut events = self
                .events
                .replay_before_limited(session_id, before_storage_cursor.as_ref(), EVENT_CHUNK + 1)
                .await?;
            if events.is_empty() {
                break false;
            }

            let chunk_has_older = events.len() > EVENT_CHUNK;
            if chunk_has_older {
                events.remove(0);
            }
            let first_seq = events.first().map(|event| event.seq);
            earliest_scanned_seq = first_seq;
            prepend_entries(&mut entries, project_events(&events));

            let has_unscanned_events = chunk_has_older || first_seq.is_some_and(|seq| seq > 0);
            if page_budget_reached(&entries, budget)?
                || !has_unscanned_events
                || (scanned_chunks >= EVENT_CHUNK_SCAN_TARGET && !entries.is_empty())
            {
                break has_unscanned_events;
            }
            before_seq = first_seq;
        };

        let (mut items, first_retained_position, dropped_items) = bounded_suffix(entries, budget)?;
        let turn_start = items
            .iter()
            .position(|entry| matches!(entry.block, ConversationBlockDto::User { .. }))
            .filter(|index| *index > 0);
        let first_retained_position = if let Some(turn_start) = turn_start {
            let position = items[turn_start].position;
            items.drain(..turn_start);
            Some(position)
        } else {
            first_retained_position
        };
        let has_older = dropped_items || turn_start.is_some() || has_unscanned_events;
        let cursor_position = if dropped_items || turn_start.is_some() {
            first_retained_position
        } else {
            earliest_scanned_seq.map(TimelinePosition::Event)
        };

        Ok(ConversationTimelinePage {
            items: items.into_iter().map(|entry| entry.block).collect(),
            older_cursor: has_older
                .then_some(cursor_position)
                .flatten()
                .map(TimelineCursor::from_position),
            has_older,
        })
    }
}

impl EventLogConversationTimeline {
    async fn fork_page_before(
        &self,
        session_id: &SessionId,
        event_seq: u64,
        item_index: usize,
        budget: PageBudget,
    ) -> Result<ConversationTimelinePage, ConversationTimelineError> {
        let after_fork = event_seq
            .checked_add(1)
            .ok_or(ConversationTimelineError::InvalidCursor)?
            .to_string();
        let event = self
            .events
            .replay_before_limited(session_id, Some(&after_fork), 1)
            .await?
            .pop()
            .filter(|event| event.seq == event_seq)
            .ok_or(ConversationTimelineError::InvalidCursor)?;
        let DurableEventPayload::SessionForked { messages, .. } = &event.payload else {
            return Err(ConversationTimelineError::InvalidCursor);
        };
        let fork_items = persisted_transcript_blocks(messages, event_seq);
        if item_index > fork_items.len() {
            return Err(ConversationTimelineError::InvalidCursor);
        }
        let entries = fork_items
            .into_iter()
            .take(item_index)
            .enumerate()
            .map(|(item_index, block)| TimelineEntry {
                position: TimelinePosition::ForkItem {
                    event_seq,
                    item_index,
                },
                block,
            })
            .collect();
        let (items, first_position, dropped_items) = bounded_suffix(entries, budget)?;
        Ok(ConversationTimelinePage {
            items: items.into_iter().map(|entry| entry.block).collect(),
            older_cursor: dropped_items
                .then_some(first_position)
                .flatten()
                .map(TimelineCursor::from_position),
            has_older: dropped_items,
        })
    }
}

struct TimelineEntry {
    position: TimelinePosition,
    block: ConversationBlockDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelinePosition {
    Event(u64),
    ForkItem { event_seq: u64, item_index: usize },
}

impl TimelinePosition {
    fn event_seq(self) -> u64 {
        match self {
            Self::Event(seq) | Self::ForkItem { event_seq: seq, .. } => seq,
        }
    }
}

fn project_events(events: &[StoredEvent]) -> Vec<TimelineEntry> {
    let mut entries = Vec::new();
    for stored in events {
        if let DurableEventPayload::SessionForked { messages, .. } = &stored.payload {
            entries.extend(
                persisted_transcript_blocks(messages, stored.seq)
                    .into_iter()
                    .enumerate()
                    .map(|(item_index, block)| TimelineEntry {
                        position: TimelinePosition::ForkItem {
                            event_seq: stored.seq,
                            item_index,
                        },
                        block,
                    }),
            );
            continue;
        }
        let block = match &stored.payload {
            DurableEventPayload::ToolCallRequested {
                call_id,
                tool_name,
                arguments,
                raw_arguments,
            } => Some(streaming_tool_call_block(
                call_id.to_string(),
                tool_name,
                raw_arguments.is_none().then_some(arguments),
            )),
            _ => block_from_payload(&Event::from(stored)),
        };
        let Some(block) = block else {
            continue;
        };
        let id = block_id(&block);
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| block_id(&entry.block) == id)
        {
            existing.block = block;
        } else {
            entries.push(TimelineEntry {
                position: TimelinePosition::Event(stored.seq),
                block,
            });
        }
    }
    entries
}

fn prepend_entries(current: &mut Vec<TimelineEntry>, older: Vec<TimelineEntry>) {
    let current_ids = current
        .iter()
        .map(|entry| block_id(&entry.block).to_owned())
        .collect::<HashSet<_>>();
    let mut merged = older
        .into_iter()
        .filter(|entry| !current_ids.contains(block_id(&entry.block)))
        .collect::<Vec<_>>();
    merged.append(current);
    *current = merged;
}

fn page_budget_reached(
    entries: &[TimelineEntry],
    budget: PageBudget,
) -> Result<bool, serde_json::Error> {
    if entries.len() >= budget.max_items {
        return Ok(true);
    }
    let mut bytes = 0usize;
    for entry in entries.iter().rev() {
        bytes = bytes.saturating_add(serde_json::to_vec(&entry.block)?.len());
        if bytes >= budget.max_bytes {
            return Ok(true);
        }
    }
    Ok(false)
}

fn bounded_suffix(
    entries: Vec<TimelineEntry>,
    budget: PageBudget,
) -> Result<(Vec<TimelineEntry>, Option<TimelinePosition>, bool), serde_json::Error> {
    let mut bytes = 0usize;
    let mut kept = 0usize;
    for entry in entries.iter().rev() {
        let item_bytes = serde_json::to_vec(&entry.block)?.len();
        if kept > 0
            && (kept >= budget.max_items || bytes.saturating_add(item_bytes) > budget.max_bytes)
        {
            break;
        }
        bytes = bytes.saturating_add(item_bytes);
        kept += 1;
    }
    let start = entries.len().saturating_sub(kept);
    let dropped = start > 0;
    let mut entries = entries;
    let retained = entries.split_off(start);
    let first_position = retained.first().map(|entry| entry.position);
    Ok((retained, first_position, dropped))
}

fn block_id(block: &ConversationBlockDto) -> &str {
    match block {
        ConversationBlockDto::User { id, .. }
        | ConversationBlockDto::Assistant { id, .. }
        | ConversationBlockDto::ToolCall { id, .. }
        | ConversationBlockDto::Error { id, .. }
        | ConversationBlockDto::Recap { id, .. }
        | ConversationBlockDto::SystemNote { id, .. }
        | ConversationBlockDto::CompactSummary { id, .. } => id,
    }
}

impl TimelineCursor {
    fn from_position(position: TimelinePosition) -> Self {
        let encoded = match position {
            TimelinePosition::Event(seq) => format!("{CURSOR_PREFIX}e:{seq:x}"),
            TimelinePosition::ForkItem {
                event_seq,
                item_index,
            } => format!("{CURSOR_PREFIX}f:{event_seq:x}:{item_index:x}"),
        };
        Self { encoded, position }
    }
}

fn decode_cursor(cursor: &str) -> Result<TimelinePosition, ConversationTimelineError> {
    let mut parts = cursor
        .strip_prefix(CURSOR_PREFIX)
        .ok_or(ConversationTimelineError::InvalidCursor)?
        .split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("e"), Some(seq), None, None) => u64::from_str_radix(seq, 16)
            .map(TimelinePosition::Event)
            .map_err(|_| ConversationTimelineError::InvalidCursor),
        (Some("f"), Some(event_seq), Some(item_index), None) => Ok(TimelinePosition::ForkItem {
            event_seq: u64::from_str_radix(event_seq, 16)
                .map_err(|_| ConversationTimelineError::InvalidCursor)?,
            item_index: usize::from_str_radix(item_index, 16)
                .map_err(|_| ConversationTimelineError::InvalidCursor)?,
        }),
        _ => Err(ConversationTimelineError::InvalidCursor),
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::{DurableEvent, DurableEventPayload};
    use astrcode_protocol::http::ConversationBlockStatusDto;
    use astrcode_storage::{SessionEventJournal, in_memory::InMemoryEventStore};

    use super::*;

    #[test]
    fn page_budget_is_a_hard_boundary_and_cursor_positions_round_trip() {
        let entries = (1..=3)
            .map(|seq| TimelineEntry {
                position: TimelinePosition::Event(seq),
                block: ConversationBlockDto::Assistant {
                    id: format!("assistant-{seq}"),
                    text: "x".repeat(600 * 1024),
                    reasoning_content: None,
                    storage_seq: Some(seq),
                    status: ConversationBlockStatusDto::Complete,
                },
            })
            .collect();
        let (page, first_position, dropped) = bounded_suffix(
            entries,
            PageBudget {
                max_items: 50,
                max_bytes: 1024 * 1024,
            },
        )
        .unwrap();

        assert_eq!(page.len(), 1);
        assert_eq!(first_position, Some(TimelinePosition::Event(3)));
        assert!(dropped);

        for position in [
            TimelinePosition::Event(42),
            TimelinePosition::ForkItem {
                event_seq: 7,
                item_index: 99,
            },
        ] {
            let cursor = TimelineCursor::from_position(position);
            assert_eq!(decode_cursor(&cursor.encoded).unwrap(), position);
        }
        assert!(decode_cursor("42").is_err());
    }

    #[tokio::test]
    async fn sparse_tail_scans_until_a_page_has_visible_content() {
        let session_id = SessionId::new("sparse-timeline");
        let store = Arc::new(InMemoryEventStore::new());
        store
            .create_session(crate::test_support::session_started_event_for_test(
                session_id.clone(),
                ".",
                "test-model",
            ))
            .await
            .unwrap();

        let mut events = vec![DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::RecapGenerated {
                text: "visible recap".into(),
                source: "test".into(),
            },
        )];
        events.extend((0..EVENT_CHUNK * EVENT_CHUNK_SCAN_TARGET + 1).map(|index| {
            DurableEvent::session(
                session_id.clone(),
                DurableEventPayload::ModelIdChanged {
                    model_id: format!("model-{index}"),
                },
            )
        }));
        store.append_events(events).await.unwrap();

        let timeline = EventLogConversationTimeline::new(store);
        let page = timeline
            .page_before(
                &session_id,
                None,
                PageBudget {
                    max_items: DEFAULT_PAGE_ITEMS,
                    max_bytes: MAX_PAGE_BYTES,
                },
            )
            .await
            .unwrap();

        assert!(matches!(
            page.items.as_slice(),
            [ConversationBlockDto::Recap { text, .. }] if text == "visible recap"
        ));
        assert!(!page.has_older);
    }
}
