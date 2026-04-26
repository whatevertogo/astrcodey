use std::collections::{BTreeSet, HashMap};

use astrcode_client::{
    ConversationBannerDto, ConversationBlockDto, ConversationBlockPatchDto,
    ConversationBlockStatusDto, ConversationChildSummaryDto, ConversationControlStateDto,
    ConversationCursorDto, ConversationDeltaDto, ConversationErrorEnvelopeDto,
    ConversationSlashCandidateDto, ConversationSnapshotResponseDto, ConversationStreamEnvelopeDto,
    PhaseDto, SessionListItem,
};

use super::{RenderState, TranscriptCell};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConversationState {
    pub sessions: Vec<SessionListItem>,
    pub active_session_id: Option<String>,
    pub active_session_title: Option<String>,
    pub cursor: Option<ConversationCursorDto>,
    pub control: Option<ConversationControlStateDto>,
    pub transcript: Vec<ConversationBlockDto>,
    pub transcript_index: HashMap<String, usize>,
    pub child_summaries: Vec<ConversationChildSummaryDto>,
    pub slash_candidates: Vec<ConversationSlashCandidateDto>,
    pub banner: Option<ConversationBannerDto>,
}

impl ConversationState {
    pub fn update_sessions(&mut self, sessions: Vec<SessionListItem>) {
        self.sessions = sessions;
    }

    pub fn activate_snapshot(
        &mut self,
        snapshot: ConversationSnapshotResponseDto,
        render: &mut RenderState,
    ) {
        self.active_session_id = Some(snapshot.session_id);
        self.active_session_title = Some(snapshot.session_title);
        self.cursor = Some(snapshot.cursor);
        self.control = Some(snapshot.control);
        self.transcript = snapshot
            .blocks
            .into_iter()
            .filter(|block| !is_step_event_block(block))
            .collect();
        self.rebuild_transcript_index();
        self.child_summaries = snapshot.child_summaries;
        self.slash_candidates = snapshot.slash_candidates;
        self.banner = snapshot.banner;
        render.mark_dirty();
    }

    pub fn apply_stream_envelope(
        &mut self,
        envelope: ConversationStreamEnvelopeDto,
        render: &mut RenderState,
        expanded_ids: &BTreeSet<String>,
    ) -> bool {
        self.cursor = Some(envelope.cursor);
        self.apply_delta(envelope.delta, render, expanded_ids)
    }

    pub fn set_banner_error(&mut self, error: ConversationErrorEnvelopeDto) {
        self.banner = Some(ConversationBannerDto { error });
    }

    pub fn clear_banner(&mut self) {
        self.banner = None;
    }

    pub fn active_phase(&self) -> Option<PhaseDto> {
        self.control.as_ref().map(|control| control.phase)
    }

    fn apply_delta(
        &mut self,
        delta: ConversationDeltaDto,
        render: &mut RenderState,
        _expanded_ids: &BTreeSet<String>,
    ) -> bool {
        match delta {
            ConversationDeltaDto::AppendBlock { block } => {
                if is_step_event_block(&block) {
                    return false;
                }
                self.transcript.push(block);
                if let Some(block) = self.transcript.last() {
                    self.transcript_index
                        .insert(block_id_of(block).to_string(), self.transcript.len() - 1);
                }
                render.mark_dirty();
                false
            },
            ConversationDeltaDto::PatchBlock { block_id, patch } => {
                if let Some((index, block)) = self.find_block_mut(block_id.as_str()) {
                    let changed = apply_block_patch(block, patch);
                    let _ = index;
                    if changed {
                        render.mark_dirty();
                    }
                } else {
                    debug_missing_block("patch", block_id.as_str());
                }
                false
            },
            ConversationDeltaDto::CompleteBlock { block_id, status } => {
                if let Some((index, block)) = self.find_block_mut(block_id.as_str()) {
                    let changed = set_block_status(block, status);
                    let _ = index;
                    if changed {
                        render.mark_dirty();
                    }
                } else {
                    debug_missing_block("complete", block_id.as_str());
                }
                false
            },
            ConversationDeltaDto::UpdateControlState { control } => {
                if self.control.as_ref() != Some(&control) {
                    self.control = Some(control);
                    render.mark_dirty();
                }
                false
            },
            ConversationDeltaDto::UpsertChildSummary { child } => {
                if let Some(existing) = self
                    .child_summaries
                    .iter_mut()
                    .find(|existing| existing.child_session_id == child.child_session_id)
                {
                    *existing = child;
                } else {
                    self.child_summaries.push(child);
                }
                false
            },
            ConversationDeltaDto::RemoveChildSummary { child_session_id } => {
                self.child_summaries
                    .retain(|child| child.child_session_id != child_session_id);
                false
            },
            ConversationDeltaDto::ReplaceSlashCandidates { candidates } => {
                self.slash_candidates = candidates;
                true
            },
            ConversationDeltaDto::SetBanner { banner } => {
                if self.banner.as_ref() != Some(&banner) {
                    self.banner = Some(banner);
                    render.mark_dirty();
                }
                false
            },
            ConversationDeltaDto::ClearBanner => {
                if self.banner.take().is_some() {
                    render.mark_dirty();
                }
                false
            },
            ConversationDeltaDto::RehydrateRequired { error } => {
                self.set_banner_error(error);
                false
            },
        }
    }

    fn rebuild_transcript_index(&mut self) {
        self.transcript_index = self
            .transcript
            .iter()
            .enumerate()
            .map(|(index, block)| (block_id_of(block).to_string(), index))
            .collect();
    }

    fn find_block_mut(&mut self, block_id: &str) -> Option<(usize, &mut ConversationBlockDto)> {
        let index = *self.transcript_index.get(block_id)?;
        self.transcript.get_mut(index).map(|block| (index, block))
    }

    pub fn project_transcript_cells(&self, expanded_ids: &BTreeSet<String>) -> Vec<TranscriptCell> {
        self.transcript
            .iter()
            .filter(|block| !is_step_event_block(block))
            .map(|block| TranscriptCell::from_block(block, expanded_ids))
            .collect()
    }

    pub fn project_transcript_cell(
        &self,
        index: usize,
        expanded_ids: &BTreeSet<String>,
    ) -> Option<TranscriptCell> {
        self.transcript
            .get(index)
            .filter(|block| !is_step_event_block(block))
            .map(|block| TranscriptCell::from_block(block, expanded_ids))
    }
}

fn is_step_event_block(block: &ConversationBlockDto) -> bool {
    matches!(block, ConversationBlockDto::PromptMetrics(_))
}

fn block_id_of(block: &ConversationBlockDto) -> &str {
    match block {
        ConversationBlockDto::User(block) => &block.id,
        ConversationBlockDto::Assistant(block) => &block.id,
        ConversationBlockDto::Thinking(block) => &block.id,
        ConversationBlockDto::PromptMetrics(block) => &block.id,
        ConversationBlockDto::Plan(block) => &block.id,
        ConversationBlockDto::ToolCall(block) => &block.id,
        ConversationBlockDto::Error(block) => &block.id,
        ConversationBlockDto::SystemNote(block) => &block.id,
        ConversationBlockDto::ChildHandoff(block) => &block.id,
    }
}

fn apply_block_patch(block: &mut ConversationBlockDto, patch: ConversationBlockPatchDto) -> bool {
    match patch {
        ConversationBlockPatchDto::AppendMarkdown { markdown } => match block {
            ConversationBlockDto::Assistant(block) => {
                normalize_markdown_append(&mut block.markdown, &markdown)
            },
            ConversationBlockDto::Thinking(block) => {
                normalize_markdown_append(&mut block.markdown, &markdown)
            },
            ConversationBlockDto::SystemNote(block) => {
                normalize_markdown_append(&mut block.markdown, &markdown)
            },
            ConversationBlockDto::User(block) => {
                normalize_markdown_append(&mut block.markdown, &markdown)
            },
            ConversationBlockDto::Plan(_) => false,
            ConversationBlockDto::ToolCall(_)
            | ConversationBlockDto::Error(_)
            | ConversationBlockDto::PromptMetrics(_)
            | ConversationBlockDto::ChildHandoff(_) => false,
        },
        ConversationBlockPatchDto::ReplaceMarkdown { markdown } => match block {
            ConversationBlockDto::Assistant(block) => {
                replace_if_changed(&mut block.markdown, markdown)
            },
            ConversationBlockDto::Thinking(block) => {
                replace_if_changed(&mut block.markdown, markdown)
            },
            ConversationBlockDto::SystemNote(block) => {
                replace_if_changed(&mut block.markdown, markdown)
            },
            ConversationBlockDto::User(block) => replace_if_changed(&mut block.markdown, markdown),
            ConversationBlockDto::Plan(_) => false,
            ConversationBlockDto::ToolCall(_)
            | ConversationBlockDto::Error(_)
            | ConversationBlockDto::PromptMetrics(_)
            | ConversationBlockDto::ChildHandoff(_) => false,
        },
        ConversationBlockPatchDto::AppendToolStream { stream, chunk } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                if enum_wire_name(&stream).as_deref() == Some("stderr") {
                    if chunk.is_empty() {
                        return false;
                    }
                    block.streams.stderr.push_str(&chunk);
                } else {
                    if chunk.is_empty() {
                        return false;
                    }
                    block.streams.stdout.push_str(&chunk);
                }
                true
            } else {
                false
            }
        },
        ConversationBlockPatchDto::ReplaceSummary { summary } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                replace_option_if_changed(&mut block.summary, summary)
            } else {
                false
            }
        },
        ConversationBlockPatchDto::ReplaceMetadata { metadata } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                replace_option_if_changed(&mut block.metadata, metadata)
            } else {
                false
            }
        },
        ConversationBlockPatchDto::ReplaceError { error } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                replace_if_changed(&mut block.error, error)
            } else {
                false
            }
        },
        ConversationBlockPatchDto::ReplaceDuration { duration_ms } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                replace_option_if_changed(&mut block.duration_ms, duration_ms)
            } else {
                false
            }
        },
        ConversationBlockPatchDto::ReplaceChildRef { child_ref } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                replace_option_if_changed(&mut block.child_ref, child_ref)
            } else {
                false
            }
        },
        ConversationBlockPatchDto::SetTruncated { truncated } => {
            if let ConversationBlockDto::ToolCall(block) = block {
                if block.truncated != truncated {
                    block.truncated = truncated;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        },
        ConversationBlockPatchDto::SetStatus { status } => set_block_status(block, status),
    }
}

fn normalize_markdown_append(current: &mut String, incoming: &str) -> bool {
    if incoming.is_empty() {
        return false;
    }

    if current.is_empty() {
        current.push_str(incoming);
        return true;
    }

    if incoming.starts_with(current.as_str()) {
        if current != incoming {
            *current = incoming.to_string();
            return true;
        }
        return false;
    }

    if current.ends_with(incoming) {
        return false;
    }

    if let Some(overlap) = longest_suffix_prefix_overlap(current.as_str(), incoming) {
        current.push_str(&incoming[overlap..]);
        return overlap < incoming.len();
    }

    current.push_str(incoming);
    true
}

fn longest_suffix_prefix_overlap(current: &str, incoming: &str) -> Option<usize> {
    let max_overlap = current.len().min(incoming.len());
    incoming
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(incoming.len()))
        .filter(|index| *index > 0 && *index <= max_overlap)
        .rev()
        .find(|index| current.ends_with(&incoming[..*index]))
}

fn enum_wire_name<T>(value: &T) -> Option<String>
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .ok()?
        .as_str()
        .map(|value| value.trim().to_string())
}

#[cfg(debug_assertions)]
fn debug_missing_block(operation: &str, block_id: &str) {
    eprintln!("astrcode-cli: ignored {operation} delta for unknown block '{block_id}'");
}

#[cfg(not(debug_assertions))]
fn debug_missing_block(_operation: &str, _block_id: &str) {}

fn set_block_status(block: &mut ConversationBlockDto, status: ConversationBlockStatusDto) -> bool {
    match block {
        ConversationBlockDto::Assistant(block) => replace_if_changed(&mut block.status, status),
        ConversationBlockDto::Thinking(block) => replace_if_changed(&mut block.status, status),
        ConversationBlockDto::Plan(_) => false,
        ConversationBlockDto::ToolCall(block) => replace_if_changed(&mut block.status, status),
        ConversationBlockDto::User(_)
        | ConversationBlockDto::Error(_)
        | ConversationBlockDto::PromptMetrics(_)
        | ConversationBlockDto::SystemNote(_)
        | ConversationBlockDto::ChildHandoff(_) => false,
    }
}

fn replace_if_changed<T: PartialEq>(slot: &mut T, next: T) -> bool {
    if *slot == next {
        false
    } else {
        *slot = next;
        true
    }
}

fn replace_option_if_changed<T: PartialEq>(slot: &mut Option<T>, next: T) -> bool {
    if slot.as_ref() == Some(&next) {
        false
    } else {
        *slot = Some(next);
        true
    }
}

#[cfg(test)]
mod tests {
    use astrcode_client::{
        ConversationAssistantBlockDto, ConversationBlockDto, ConversationBlockPatchDto,
        ConversationBlockStatusDto, ConversationCursorDto, ConversationDeltaDto,
        ConversationStreamEnvelopeDto,
    };

    use super::{ConversationState, normalize_markdown_append};
    use crate::state::RenderState;

    #[test]
    fn append_markdown_replaces_with_cumulative_body() {
        let mut current = "你好".to_string();
        normalize_markdown_append(&mut current, "你好，世界");
        assert_eq!(current, "你好，世界");
    }

    #[test]
    fn append_markdown_ignores_replayed_suffix() {
        let mut current = "你好，世界".to_string();
        normalize_markdown_append(&mut current, "世界");
        assert_eq!(current, "你好，世界");
    }

    #[test]
    fn append_markdown_appends_only_non_overlapping_suffix() {
        let mut current = "你好，世".to_string();
        normalize_markdown_append(&mut current, "世界");
        assert_eq!(current, "你好，世界");
    }

    #[test]
    fn append_markdown_keeps_true_incremental_append() {
        let mut current = "你好".to_string();
        normalize_markdown_append(&mut current, "，世界");
        assert_eq!(current, "你好，世界");
    }

    #[test]
    fn duplicate_markdown_replay_does_not_mark_surface_dirty() {
        let mut conversation = ConversationState {
            transcript: vec![ConversationBlockDto::Assistant(
                ConversationAssistantBlockDto {
                    id: "assistant-1".to_string(),
                    turn_id: Some("turn-1".to_string()),
                    status: ConversationBlockStatusDto::Streaming,
                    markdown: "你好，世界".to_string(),
                    step_index: None,
                },
            )],
            transcript_index: [("assistant-1".to_string(), 0)].into_iter().collect(),
            ..Default::default()
        };
        let mut render = RenderState::default();
        render.take_frame_dirty();

        conversation.apply_stream_envelope(
            ConversationStreamEnvelopeDto {
                session_id: "session-1".to_string(),
                cursor: ConversationCursorDto("1.1".to_string()),
                step_progress: Default::default(),
                delta: ConversationDeltaDto::PatchBlock {
                    block_id: "assistant-1".to_string(),
                    patch: ConversationBlockPatchDto::AppendMarkdown {
                        markdown: "世界".to_string(),
                    },
                },
            },
            &mut render,
            &Default::default(),
        );

        assert!(!render.take_frame_dirty());
    }
}
