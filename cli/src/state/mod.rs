mod conversation;
mod debug;
mod interaction;
mod render;
mod shell;
mod thinking;
mod transcript_cell;

use std::{path::PathBuf, time::Duration};

use astrcode_client::{
    ConversationErrorEnvelopeDto, ConversationSlashCandidateDto, ConversationSnapshotResponseDto,
    ConversationStreamEnvelopeDto, CurrentModelInfoDto, ModeSummaryDto, ModelOptionDto, PhaseDto,
    SessionListItem,
};
pub use conversation::ConversationState;
pub use debug::DebugChannelState;
pub use interaction::{
    ComposerState, InteractionState, PaletteSelection, PaletteState, PaneFocus, ResumePaletteState,
    SlashPaletteState, StatusLine,
};
pub use render::{
    ActiveOverlay, RenderState, StreamViewState, WrappedLine, WrappedLineRewrapPolicy,
    WrappedLineStyle, WrappedSpan, WrappedSpanStyle,
};
pub use shell::ShellState;
pub use thinking::{ThinkingPlaybackDriver, ThinkingPresentationState, ThinkingSnippetPool};
pub use transcript_cell::{TranscriptCell, TranscriptCellKind, TranscriptCellStatus};

use crate::capability::TerminalCapabilities;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamRenderMode {
    #[default]
    Smooth,
    CatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliState {
    pub shell: ShellState,
    pub conversation: ConversationState,
    pub interaction: InteractionState,
    pub render: RenderState,
    pub stream_view: StreamViewState,
    pub debug: DebugChannelState,
    pub thinking_pool: ThinkingSnippetPool,
    pub thinking_playback: ThinkingPlaybackDriver,
}

impl CliState {
    pub fn new(
        connection_origin: String,
        working_dir: Option<PathBuf>,
        capabilities: TerminalCapabilities,
    ) -> Self {
        Self {
            shell: ShellState::new(connection_origin, working_dir, capabilities),
            thinking_pool: ThinkingSnippetPool::default(),
            thinking_playback: ThinkingPlaybackDriver::default(),
            ..Default::default()
        }
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.interaction.set_status(message);
        self.render.mark_dirty();
    }

    pub fn set_error_status(&mut self, message: impl Into<String>) {
        self.interaction.set_error_status(message);
        self.render.mark_dirty();
    }

    pub fn set_stream_mode(
        &mut self,
        mode: StreamRenderMode,
        pending: usize,
        oldest: Duration,
    ) -> bool {
        let changed = self.stream_view.mode != mode || self.stream_view.pending_chunks != pending;
        self.stream_view.update(mode, pending, oldest);
        changed
    }

    pub fn note_terminal_resize(&mut self, width: u16, height: u16) {
        self.render.note_terminal_resize(width, height);
    }

    pub fn push_input(&mut self, ch: char) {
        self.interaction.push_input(ch);
        self.render.mark_dirty();
    }

    pub fn append_input(&mut self, value: &str) {
        self.interaction.append_input(value);
        self.render.mark_dirty();
    }

    pub fn insert_newline(&mut self) {
        self.interaction.insert_newline();
        self.render.mark_dirty();
    }

    pub fn pop_input(&mut self) {
        self.interaction.pop_input();
        self.render.mark_dirty();
    }

    pub fn delete_input(&mut self) {
        self.interaction.delete_input();
        self.render.mark_dirty();
    }

    pub fn move_cursor_left(&mut self) {
        self.interaction.move_cursor_left();
        self.render.mark_dirty();
    }

    pub fn move_cursor_right(&mut self) {
        self.interaction.move_cursor_right();
        self.render.mark_dirty();
    }

    pub fn move_cursor_home(&mut self) {
        self.interaction.move_cursor_home();
        self.render.mark_dirty();
    }

    pub fn move_cursor_end(&mut self) {
        self.interaction.move_cursor_end();
        self.render.mark_dirty();
    }

    pub fn replace_input(&mut self, input: impl Into<String>) {
        self.interaction.replace_input(input);
        self.render.mark_dirty();
    }

    pub fn take_input(&mut self) -> String {
        let input = self.interaction.take_input();
        self.render.mark_dirty();
        input
    }

    pub fn cycle_focus_forward(&mut self) {
        self.interaction.cycle_focus_forward();
        self.render.mark_dirty();
    }

    pub fn cycle_focus_backward(&mut self) {
        self.interaction.cycle_focus_backward();
        self.render.mark_dirty();
    }

    pub fn transcript_next(&mut self) {
        self.interaction
            .transcript_next(self.conversation.transcript.len());
        self.render.mark_dirty();
    }

    pub fn transcript_prev(&mut self) {
        self.interaction
            .transcript_prev(self.conversation.transcript.len());
        self.render.mark_dirty();
    }

    pub fn transcript_cells(&self) -> Vec<TranscriptCell> {
        self.conversation
            .project_transcript_cells(&self.interaction.transcript.expanded_cells)
    }

    pub fn browser_transcript_cells(&self) -> Vec<TranscriptCell> {
        self.transcript_cells()
            .into_iter()
            .filter(transcript_cell_visible_in_browser)
            .collect()
    }

    pub fn selected_transcript_cell(&self) -> Option<TranscriptCell> {
        self.conversation.project_transcript_cell(
            self.interaction.transcript.selected_cell,
            &self.interaction.transcript.expanded_cells,
        )
    }

    pub fn selected_browser_cell(&self) -> Option<TranscriptCell> {
        self.browser_transcript_cells()
            .into_iter()
            .nth(self.interaction.browser.selected_cell)
    }

    pub fn is_cell_expanded(&self, cell_id: &str) -> bool {
        self.interaction.is_cell_expanded(cell_id)
    }

    pub fn selected_cell_is_thinking(&self) -> bool {
        self.selected_transcript_cell()
            .is_some_and(|cell| matches!(cell.kind, TranscriptCellKind::Thinking { .. }))
    }

    pub fn toggle_selected_cell_expanded(&mut self) {
        let selected = if self.interaction.browser.open {
            self.selected_browser_cell()
        } else {
            self.selected_transcript_cell()
        };
        if let Some(cell_id) = selected.map(|cell| cell.id.clone()) {
            self.interaction.toggle_cell_expanded(cell_id.as_str());
            self.render.mark_dirty();
        }
    }

    pub fn clear_surface_state(&mut self) {
        self.interaction.clear_surface_state();
        self.sync_overlay_state();
        self.render.mark_dirty();
    }

    pub fn update_sessions(&mut self, sessions: Vec<SessionListItem>) {
        self.conversation.update_sessions(sessions);
        self.interaction
            .sync_resume_items(self.conversation.sessions.clone());
        self.render.mark_dirty();
    }

    pub fn update_current_model(&mut self, current_model: CurrentModelInfoDto) {
        if self.shell.current_model.as_ref() != Some(&current_model) {
            self.shell.current_model = Some(current_model);
            self.render.mark_dirty();
        }
    }

    pub fn update_model_options(&mut self, model_options: Vec<ModelOptionDto>) {
        if self.shell.model_options != model_options {
            self.shell.model_options = model_options.clone();
            self.interaction.sync_model_items(model_options);
            self.render.mark_dirty();
        }
    }

    pub fn update_modes(&mut self, modes: Vec<ModeSummaryDto>) {
        if self.shell.available_modes != modes {
            self.shell.available_modes = modes;
            self.render.mark_dirty();
        }
    }

    pub fn set_resume_query(&mut self, query: impl Into<String>, items: Vec<SessionListItem>) {
        self.interaction.set_resume_palette(query, items);
        self.render.mark_dirty();
    }

    pub fn set_slash_query(
        &mut self,
        query: impl Into<String>,
        items: Vec<ConversationSlashCandidateDto>,
    ) {
        self.interaction.set_slash_palette(query, items);
        self.render.mark_dirty();
    }

    pub fn set_model_query(&mut self, query: impl Into<String>, items: Vec<ModelOptionDto>) {
        self.interaction.set_model_palette(query, items);
        self.render.mark_dirty();
    }

    pub fn close_palette(&mut self) {
        self.interaction.close_palette();
        self.render.mark_dirty();
    }

    pub fn toggle_browser(&mut self) {
        if self.interaction.browser.open {
            self.interaction.close_browser();
        } else {
            self.interaction
                .open_browser(self.browser_transcript_cells().len());
        }
        self.sync_overlay_state();
        self.render.mark_dirty();
    }

    pub fn browser_next(&mut self, page_size: usize) {
        self.interaction
            .browser_next(self.browser_transcript_cells().len(), page_size);
        self.render.mark_dirty();
    }

    pub fn browser_prev(&mut self, page_size: usize) {
        self.interaction
            .browser_prev(self.browser_transcript_cells().len(), page_size);
        self.render.mark_dirty();
    }

    pub fn browser_first(&mut self) {
        self.interaction
            .browser_first(self.browser_transcript_cells().len());
        self.render.mark_dirty();
    }

    pub fn browser_last(&mut self) {
        self.interaction
            .browser_last(self.browser_transcript_cells().len());
        self.render.mark_dirty();
    }

    pub fn palette_next(&mut self) {
        self.interaction.palette_next();
        self.render.mark_dirty();
    }

    pub fn palette_prev(&mut self) {
        self.interaction.palette_prev();
        self.render.mark_dirty();
    }

    pub fn selected_palette(&self) -> Option<PaletteSelection> {
        self.interaction.selected_palette()
    }

    pub fn activate_snapshot(&mut self, snapshot: ConversationSnapshotResponseDto) {
        self.conversation
            .activate_snapshot(snapshot, &mut self.render);
        self.interaction.reset_for_snapshot();
        self.interaction
            .sync_transcript_cells(self.conversation.transcript.len());
        self.thinking_playback
            .sync_session(self.conversation.active_session_id.as_deref());
        self.sync_overlay_state();
        self.render.mark_dirty();
    }

    pub fn apply_stream_envelope(&mut self, envelope: ConversationStreamEnvelopeDto) {
        let expanded_ids = &self.interaction.transcript.expanded_cells;
        let slash_candidates_changed =
            self.conversation
                .apply_stream_envelope(envelope, &mut self.render, expanded_ids);
        self.interaction
            .sync_transcript_cells(self.conversation.transcript.len());
        if slash_candidates_changed {
            self.interaction
                .sync_slash_items(self.conversation.slash_candidates.clone());
        }
        self.render.mark_dirty();
    }

    pub fn set_banner_error(&mut self, error: ConversationErrorEnvelopeDto) {
        self.conversation.set_banner_error(error);
        self.interaction.set_focus(PaneFocus::Composer);
        self.render.mark_dirty();
    }

    pub fn clear_banner(&mut self) {
        self.conversation.clear_banner();
        self.render.mark_dirty();
    }

    pub fn active_phase(&self) -> Option<PhaseDto> {
        self.conversation.active_phase()
    }

    pub fn push_debug_line(&mut self, line: impl Into<String>) {
        self.debug.push(line);
    }

    pub fn advance_thinking_playback(&mut self) -> bool {
        if self.should_animate_thinking_playback() {
            self.thinking_playback.advance();
            self.render.mark_dirty();
            return true;
        }
        false
    }

    fn sync_overlay_state(&mut self) {
        let overlay = if self.interaction.browser.open {
            ActiveOverlay::Browser
        } else {
            ActiveOverlay::None
        };
        self.render.set_active_overlay(overlay);
    }

    fn should_animate_thinking_playback(&self) -> bool {
        if self.transcript_cells().iter().any(|cell| {
            matches!(
                cell.kind,
                TranscriptCellKind::Thinking {
                    status: TranscriptCellStatus::Streaming,
                    ..
                }
            )
        }) {
            return true;
        }

        let Some(control) = &self.conversation.control else {
            return false;
        };
        if control.active_turn_id.is_none() {
            return false;
        }
        if !matches!(
            control.phase,
            PhaseDto::Thinking | PhaseDto::CallingTool | PhaseDto::Streaming
        ) {
            return false;
        }

        !self.transcript_cells().iter().any(|cell| match &cell.kind {
            TranscriptCellKind::Thinking { status, .. } => {
                matches!(
                    status,
                    TranscriptCellStatus::Streaming | TranscriptCellStatus::Complete
                )
            },
            TranscriptCellKind::Assistant { status, body } => {
                matches!(status, TranscriptCellStatus::Streaming) && !body.trim().is_empty()
            },
            TranscriptCellKind::ToolCall { status, .. } => {
                matches!(status, TranscriptCellStatus::Streaming)
            },
            _ => false,
        })
    }
}

fn transcript_cell_visible_in_browser(cell: &TranscriptCell) -> bool {
    match &cell.kind {
        TranscriptCellKind::Assistant { status, .. }
        | TranscriptCellKind::Thinking { status, .. }
        | TranscriptCellKind::ToolCall { status, .. } => {
            !matches!(status, TranscriptCellStatus::Streaming)
        },
        TranscriptCellKind::User { .. }
        | TranscriptCellKind::Error { .. }
        | TranscriptCellKind::SystemNote { .. }
        | TranscriptCellKind::ChildHandoff { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_client::{
        ConversationAssistantBlockDto, ConversationBlockDto, ConversationBlockPatchDto,
        ConversationBlockStatusDto, ConversationControlStateDto, ConversationCursorDto,
        ConversationDeltaDto, ConversationSlashActionKindDto,
    };

    use super::*;
    use crate::capability::{ColorLevel, GlyphMode};

    fn sample_snapshot() -> ConversationSnapshotResponseDto {
        ConversationSnapshotResponseDto {
            session_id: "session-1".to_string(),
            session_title: "Session 1".to_string(),
            cursor: ConversationCursorDto("1.2".to_string()),
            phase: PhaseDto::Idle,
            control: ConversationControlStateDto {
                phase: PhaseDto::Idle,
                can_submit_prompt: true,
                can_request_compact: true,
                compact_pending: false,
                compacting: false,
                current_mode_id: "code".to_string(),
                active_turn_id: None,
                last_compact_meta: None,
                active_plan: None,
                active_tasks: None,
            },
            step_progress: Default::default(),
            blocks: vec![ConversationBlockDto::Assistant(
                ConversationAssistantBlockDto {
                    id: "assistant-1".to_string(),
                    turn_id: Some("turn-1".to_string()),
                    status: ConversationBlockStatusDto::Streaming,
                    markdown: "hello".to_string(),
                    step_index: None,
                },
            )],
            child_summaries: Vec::new(),
            slash_candidates: vec![ConversationSlashCandidateDto {
                id: "review".to_string(),
                title: "Review".to_string(),
                description: "review skill".to_string(),
                keywords: vec!["review".to_string()],
                action_kind: ConversationSlashActionKindDto::InsertText,
                action_value: "/review".to_string(),
            }],
            banner: None,
        }
    }

    fn capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::TrueColor,
            glyphs: GlyphMode::Unicode,
            alt_screen: true,
            mouse: true,
            bracketed_paste: true,
        }
    }

    #[test]
    fn applies_snapshot_and_stream_deltas() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.activate_snapshot(sample_snapshot());
        state.apply_stream_envelope(ConversationStreamEnvelopeDto {
            session_id: "session-1".to_string(),
            cursor: ConversationCursorDto("1.3".to_string()),
            step_progress: Default::default(),
            delta: ConversationDeltaDto::PatchBlock {
                block_id: "assistant-1".to_string(),
                patch: ConversationBlockPatchDto::AppendMarkdown {
                    markdown: " world".to_string(),
                },
            },
        });

        let ConversationBlockDto::Assistant(block) = &state.conversation.transcript[0] else {
            panic!("assistant block should remain present");
        };
        assert_eq!(block.markdown, "hello world");
        assert_eq!(
            state
                .conversation
                .cursor
                .as_ref()
                .map(|cursor| cursor.0.as_str()),
            Some("1.3")
        );
    }

    #[test]
    fn replace_markdown_patch_overwrites_streamed_content() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.activate_snapshot(sample_snapshot());
        state.apply_stream_envelope(ConversationStreamEnvelopeDto {
            session_id: "session-1".to_string(),
            cursor: ConversationCursorDto("1.4".to_string()),
            step_progress: Default::default(),
            delta: ConversationDeltaDto::PatchBlock {
                block_id: "assistant-1".to_string(),
                patch: ConversationBlockPatchDto::ReplaceMarkdown {
                    markdown: "replaced".to_string(),
                },
            },
        });

        let ConversationBlockDto::Assistant(block) = &state.conversation.transcript[0] else {
            panic!("assistant block should remain present");
        };
        assert_eq!(block.markdown, "replaced");
    }

    #[test]
    fn palette_selection_tracks_resume_and_slash_items() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.set_slash_query(
            "review",
            vec![ConversationSlashCandidateDto {
                id: "review".to_string(),
                title: "Review".to_string(),
                description: "review skill".to_string(),
                keywords: vec!["review".to_string()],
                action_kind: ConversationSlashActionKindDto::InsertText,
                action_value: "/review".to_string(),
            }],
        );

        assert!(matches!(
            state.selected_palette(),
            Some(PaletteSelection::SlashCandidate(_))
        ));
        state.set_resume_query("repo", Vec::new());
        assert!(matches!(state.interaction.palette, PaletteState::Resume(_)));
    }

    #[test]
    fn resize_invalidates_wrap_cache() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.note_terminal_resize(100, 40);

        assert!(state.render.frame_dirty);
    }

    #[test]
    fn viewport_resize_marks_surface_dirty() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.render.take_frame_dirty();
        state.note_terminal_resize(80, 6);
        assert!(state.render.take_frame_dirty());
    }

    #[test]
    fn browser_toggle_marks_surface_dirty() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.activate_snapshot(sample_snapshot());
        state.render.take_frame_dirty();

        state.toggle_browser();

        assert!(state.interaction.browser.open);
        assert!(state.render.take_frame_dirty());
    }

    #[test]
    fn ticking_advances_streaming_thinking() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.conversation.control = Some(ConversationControlStateDto {
            phase: PhaseDto::Thinking,
            can_submit_prompt: true,
            can_request_compact: true,
            compact_pending: false,
            compacting: false,
            current_mode_id: "code".to_string(),
            active_turn_id: Some("turn-1".to_string()),
            last_compact_meta: None,
            active_plan: None,
            active_tasks: None,
        });
        let frame = state.thinking_playback.frame;
        state.advance_thinking_playback();
        assert_eq!(state.thinking_playback.frame, frame.wrapping_add(1));
    }

    #[test]
    fn browser_filters_out_streaming_cells() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.conversation.transcript = vec![
            ConversationBlockDto::Assistant(ConversationAssistantBlockDto {
                id: "assistant-streaming".to_string(),
                turn_id: Some("turn-1".to_string()),
                status: ConversationBlockStatusDto::Streaming,
                markdown: "draft".to_string(),
                step_index: None,
            }),
            ConversationBlockDto::Assistant(ConversationAssistantBlockDto {
                id: "assistant-complete".to_string(),
                turn_id: Some("turn-1".to_string()),
                status: ConversationBlockStatusDto::Complete,
                markdown: "done".to_string(),
                step_index: None,
            }),
        ];

        let browser_cells = state.browser_transcript_cells();
        assert_eq!(browser_cells.len(), 1);
        assert_eq!(browser_cells[0].id, "assistant-complete");
    }
}
