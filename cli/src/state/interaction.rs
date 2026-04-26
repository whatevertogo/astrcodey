use std::collections::BTreeSet;

use astrcode_client::{ConversationSlashCandidateDto, ModelOptionDto, SessionListItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneFocus {
    #[default]
    Composer,
    Palette,
    Browser,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerState {
    pub input: String,
    pub cursor: usize,
}

impl ComposerState {
    pub fn line_count(&self) -> usize {
        self.input.lines().count().max(1)
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    pub fn as_str(&self) -> &str {
        self.input.as_str()
    }

    pub fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_str(&mut self, value: &str) {
        self.input.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        let Some(start) = previous_boundary(self.input.as_str(), self.cursor) else {
            return;
        };
        self.input.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn delete_forward(&mut self) {
        let Some(end) = next_boundary(self.input.as_str(), self.cursor) else {
            return;
        };
        self.input.drain(self.cursor..end);
    }

    pub fn move_left(&mut self) {
        if let Some(cursor) = previous_boundary(self.input.as_str(), self.cursor) {
            self.cursor = cursor;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(cursor) = next_boundary(self.input.as_str(), self.cursor) {
            self.cursor = cursor;
        }
    }

    pub fn move_home(&mut self) {
        let line_start = self
            .input
            .get(..self.cursor)
            .and_then(|value| value.rfind('\n').map(|index| index + 1))
            .unwrap_or(0);
        self.cursor = line_start;
    }

    pub fn move_end(&mut self) {
        let line_end = self
            .input
            .get(self.cursor..)
            .and_then(|value| value.find('\n').map(|index| self.cursor + index))
            .unwrap_or(self.input.len());
        self.cursor = line_end;
    }

    pub fn replace(&mut self, input: impl Into<String>) {
        self.input = input.into();
        self.cursor = self.input.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }
}

fn previous_boundary(input: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    input
        .get(..cursor)?
        .char_indices()
        .last()
        .map(|(index, _)| index)
}

fn next_boundary(input: &str, cursor: usize) -> Option<usize> {
    if cursor >= input.len() {
        return None;
    }
    let ch = input.get(cursor..)?.chars().next()?;
    Some(cursor + ch.len_utf8())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashPaletteState {
    pub query: String,
    pub items: Vec<ConversationSlashCandidateDto>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePaletteState {
    pub query: String,
    pub items: Vec<SessionListItem>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPaletteState {
    pub query: String,
    pub items: Vec<ModelOptionDto>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PaletteState {
    #[default]
    Closed,
    Slash(SlashPaletteState),
    Resume(ResumePaletteState),
    Model(ModelPaletteState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteSelection {
    ResumeSession(String),
    SlashCandidate(ConversationSlashCandidateDto),
    ModelOption(ModelOptionDto),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub message: String,
    pub is_error: bool,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            message: "ready".to_string(),
            is_error: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptState {
    pub selected_cell: usize,
    pub expanded_cells: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrowserState {
    pub open: bool,
    pub selected_cell: usize,
    pub last_seen_cell_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InteractionState {
    pub status: StatusLine,
    pub pane_focus: PaneFocus,
    pub last_non_palette_focus: PaneFocus,
    pub composer: ComposerState,
    pub palette: PaletteState,
    pub transcript: TranscriptState,
    pub browser: BrowserState,
}

impl InteractionState {
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = StatusLine {
            message: message.into(),
            is_error: false,
        };
    }

    pub fn set_error_status(&mut self, message: impl Into<String>) {
        self.status = StatusLine {
            message: message.into(),
            is_error: true,
        };
    }

    pub fn push_input(&mut self, ch: char) {
        self.set_focus(PaneFocus::Composer);
        self.composer.insert_char(ch);
    }

    pub fn append_input(&mut self, value: &str) {
        self.set_focus(PaneFocus::Composer);
        self.composer.insert_str(value);
    }

    pub fn insert_newline(&mut self) {
        self.set_focus(PaneFocus::Composer);
        self.composer.insert_newline();
    }

    pub fn pop_input(&mut self) {
        self.composer.backspace();
    }

    pub fn delete_input(&mut self) {
        self.composer.delete_forward();
    }

    pub fn move_cursor_left(&mut self) {
        self.composer.move_left();
    }

    pub fn move_cursor_right(&mut self) {
        self.composer.move_right();
    }

    pub fn move_cursor_home(&mut self) {
        self.composer.move_home();
    }

    pub fn move_cursor_end(&mut self) {
        self.composer.move_end();
    }

    pub fn replace_input(&mut self, input: impl Into<String>) {
        self.set_focus(PaneFocus::Composer);
        self.composer.replace(input);
    }

    pub fn take_input(&mut self) -> String {
        self.composer.take()
    }

    pub fn cycle_focus_forward(&mut self) {
        self.set_focus(match self.pane_focus {
            PaneFocus::Composer => PaneFocus::Composer,
            PaneFocus::Palette => PaneFocus::Palette,
            PaneFocus::Browser => PaneFocus::Browser,
        });
    }

    pub fn cycle_focus_backward(&mut self) {
        self.set_focus(match self.pane_focus {
            PaneFocus::Composer => PaneFocus::Composer,
            PaneFocus::Palette => PaneFocus::Palette,
            PaneFocus::Browser => PaneFocus::Browser,
        });
    }

    pub fn set_focus(&mut self, focus: PaneFocus) {
        self.pane_focus = focus;
        if !matches!(focus, PaneFocus::Palette | PaneFocus::Browser) {
            self.last_non_palette_focus = focus;
        }
    }

    pub fn transcript_next(&mut self, cell_count: usize) {
        if cell_count == 0 {
            return;
        }
        self.transcript.selected_cell = (self.transcript.selected_cell + 1) % cell_count;
    }

    pub fn transcript_prev(&mut self, cell_count: usize) {
        if cell_count == 0 {
            return;
        }
        self.transcript.selected_cell =
            (self.transcript.selected_cell + cell_count - 1) % cell_count;
    }

    pub fn sync_transcript_cells(&mut self, cell_count: usize) {
        if cell_count == 0 {
            self.transcript.selected_cell = 0;
            self.transcript.expanded_cells.clear();
            return;
        }
        if self.transcript.selected_cell >= cell_count {
            self.transcript.selected_cell = cell_count - 1;
        }
    }

    pub fn toggle_cell_expanded(&mut self, cell_id: &str) {
        if !self.transcript.expanded_cells.insert(cell_id.to_string()) {
            self.transcript.expanded_cells.remove(cell_id);
        }
    }

    pub fn is_cell_expanded(&self, cell_id: &str) -> bool {
        self.transcript.expanded_cells.contains(cell_id)
    }

    pub fn reset_for_snapshot(&mut self) {
        self.palette = PaletteState::Closed;
        self.transcript = TranscriptState::default();
        self.browser = BrowserState::default();
        self.set_focus(PaneFocus::Composer);
    }

    pub fn open_browser(&mut self, cell_count: usize) {
        self.browser.open = true;
        if cell_count == 0 {
            self.browser.selected_cell = 0;
        } else if self.browser.selected_cell >= cell_count {
            self.browser.selected_cell = cell_count - 1;
        }
        self.browser.last_seen_cell_count = cell_count;
        self.transcript.selected_cell = self.browser.selected_cell;
        self.pane_focus = PaneFocus::Browser;
    }

    pub fn close_browser(&mut self) {
        self.browser.open = false;
        self.set_focus(PaneFocus::Composer);
    }

    pub fn browser_next(&mut self, cell_count: usize, page_size: usize) {
        if cell_count == 0 {
            return;
        }
        self.open_browser(cell_count);
        self.browser.selected_cell =
            (self.browser.selected_cell + page_size.max(1)).min(cell_count - 1);
        if self.browser.selected_cell == cell_count - 1 {
            self.browser.last_seen_cell_count = cell_count;
        }
        self.transcript.selected_cell = self.browser.selected_cell;
    }

    pub fn browser_prev(&mut self, cell_count: usize, page_size: usize) {
        if cell_count == 0 {
            return;
        }
        self.open_browser(cell_count);
        self.browser.selected_cell = self.browser.selected_cell.saturating_sub(page_size.max(1));
        self.transcript.selected_cell = self.browser.selected_cell;
    }

    pub fn browser_first(&mut self, cell_count: usize) {
        if cell_count == 0 {
            return;
        }
        self.open_browser(cell_count);
        self.browser.selected_cell = 0;
        self.transcript.selected_cell = 0;
    }

    pub fn browser_last(&mut self, cell_count: usize) {
        if cell_count == 0 {
            return;
        }
        self.open_browser(cell_count);
        self.browser.selected_cell = cell_count - 1;
        self.browser.last_seen_cell_count = cell_count;
        self.transcript.selected_cell = self.browser.selected_cell;
    }

    pub fn set_resume_palette(&mut self, query: impl Into<String>, items: Vec<SessionListItem>) {
        self.palette = PaletteState::Resume(ResumePaletteState {
            query: query.into(),
            items,
            selected: 0,
        });
        self.pane_focus = PaneFocus::Palette;
    }

    pub fn sync_resume_items(&mut self, items: Vec<SessionListItem>) {
        if let PaletteState::Resume(resume) = &mut self.palette {
            resume.items = items;
            if resume.selected >= resume.items.len() {
                resume.selected = 0;
            }
        }
    }

    pub fn set_slash_palette(
        &mut self,
        query: impl Into<String>,
        items: Vec<ConversationSlashCandidateDto>,
    ) {
        self.palette = PaletteState::Slash(SlashPaletteState {
            query: query.into(),
            items,
            selected: 0,
        });
        self.pane_focus = PaneFocus::Palette;
    }

    pub fn set_model_palette(&mut self, query: impl Into<String>, items: Vec<ModelOptionDto>) {
        self.palette = PaletteState::Model(ModelPaletteState {
            query: query.into(),
            items,
            selected: 0,
        });
        self.pane_focus = PaneFocus::Palette;
    }

    pub fn sync_model_items(&mut self, items: Vec<ModelOptionDto>) {
        if let PaletteState::Model(palette) = &mut self.palette {
            palette.items = items;
            if palette.selected >= palette.items.len() {
                palette.selected = 0;
            }
        }
    }

    pub fn sync_slash_items(&mut self, items: Vec<ConversationSlashCandidateDto>) {
        if let PaletteState::Slash(palette) = &mut self.palette {
            palette.items = items;
            if palette.selected >= palette.items.len() {
                palette.selected = 0;
            }
        }
    }

    pub fn close_palette(&mut self) {
        self.palette = PaletteState::Closed;
        self.set_focus(self.last_non_palette_focus);
    }

    pub fn has_palette(&self) -> bool {
        !matches!(self.palette, PaletteState::Closed)
    }

    pub fn palette_next(&mut self) {
        match &mut self.palette {
            PaletteState::Resume(resume) if !resume.items.is_empty() => {
                resume.selected = (resume.selected + 1) % resume.items.len();
            },
            PaletteState::Slash(palette) if !palette.items.is_empty() => {
                palette.selected = (palette.selected + 1) % palette.items.len();
            },
            PaletteState::Model(palette) if !palette.items.is_empty() => {
                palette.selected = (palette.selected + 1) % palette.items.len();
            },
            PaletteState::Closed
            | PaletteState::Resume(_)
            | PaletteState::Slash(_)
            | PaletteState::Model(_) => {},
        }
    }

    pub fn palette_prev(&mut self) {
        match &mut self.palette {
            PaletteState::Resume(resume) if !resume.items.is_empty() => {
                resume.selected =
                    (resume.selected + resume.items.len().saturating_sub(1)) % resume.items.len();
            },
            PaletteState::Slash(palette) if !palette.items.is_empty() => {
                palette.selected = (palette.selected + palette.items.len().saturating_sub(1))
                    % palette.items.len();
            },
            PaletteState::Model(palette) if !palette.items.is_empty() => {
                palette.selected = (palette.selected + palette.items.len().saturating_sub(1))
                    % palette.items.len();
            },
            PaletteState::Closed
            | PaletteState::Resume(_)
            | PaletteState::Slash(_)
            | PaletteState::Model(_) => {},
        }
    }

    pub fn selected_palette(&self) -> Option<PaletteSelection> {
        match &self.palette {
            PaletteState::Resume(resume) => resume
                .items
                .get(resume.selected)
                .map(|item| PaletteSelection::ResumeSession(item.session_id.clone())),
            PaletteState::Slash(palette) => palette
                .items
                .get(palette.selected)
                .cloned()
                .map(PaletteSelection::SlashCandidate),
            PaletteState::Model(palette) => palette
                .items
                .get(palette.selected)
                .cloned()
                .map(PaletteSelection::ModelOption),
            PaletteState::Closed => None,
        }
    }

    pub fn clear_surface_state(&mut self) {
        match self.pane_focus {
            PaneFocus::Composer => {
                self.status = StatusLine::default();
                self.transcript.expanded_cells.clear();
            },
            PaneFocus::Palette => self.close_palette(),
            PaneFocus::Browser => self.close_browser(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_flow_cycles_two_surfaces() {
        let mut state = InteractionState::default();
        state.cycle_focus_forward();
        assert_eq!(state.pane_focus, PaneFocus::Composer);
        state.cycle_focus_forward();
        assert_eq!(state.pane_focus, PaneFocus::Composer);
    }

    #[test]
    fn close_palette_restores_previous_focus() {
        let mut state = InteractionState::default();
        state.set_slash_palette("", Vec::new());
        assert_eq!(state.pane_focus, PaneFocus::Palette);
        state.close_palette();
        assert_eq!(state.pane_focus, PaneFocus::Composer);
    }

    #[test]
    fn transcript_expansion_toggles_by_cell_id() {
        let mut state = InteractionState::default();
        state.toggle_cell_expanded("assistant-1");
        assert!(state.is_cell_expanded("assistant-1"));
        state.toggle_cell_expanded("assistant-1");
        assert!(!state.is_cell_expanded("assistant-1"));
    }

    #[test]
    fn browser_open_close_and_navigation_track_selected_cell() {
        let mut state = InteractionState::default();
        state.open_browser(4);
        assert!(state.browser.open);
        assert_eq!(state.pane_focus, PaneFocus::Browser);

        state.browser_next(4, 1);
        assert_eq!(state.browser.selected_cell, 1);
        state.browser_next(4, 5);
        assert_eq!(state.browser.selected_cell, 3);
        state.browser_prev(4, 2);
        assert_eq!(state.browser.selected_cell, 1);
        state.browser_first(4);
        assert_eq!(state.browser.selected_cell, 0);
        state.browser_last(4);
        assert_eq!(state.browser.selected_cell, 3);

        state.close_browser();
        assert!(!state.browser.open);
        assert_eq!(state.pane_focus, PaneFocus::Composer);
    }

    #[test]
    fn composer_backspace_respects_cursor_position() {
        let mut state = InteractionState::default();
        state.replace_input("abcd");
        state.move_cursor_left();
        state.move_cursor_left();
        state.pop_input();
        assert_eq!(state.composer.as_str(), "acd");
        assert_eq!(state.composer.cursor, 1);
    }

    #[test]
    fn composer_delete_forward_respects_cursor_position() {
        let mut state = InteractionState::default();
        state.replace_input("abcd");
        state.move_cursor_left();
        state.move_cursor_left();
        state.delete_input();
        assert_eq!(state.composer.as_str(), "abd");
        assert_eq!(state.composer.cursor, 2);
    }

    #[test]
    fn transcript_navigation_updates_selected_cell() {
        let mut state = InteractionState::default();
        state.transcript_next(4);
        assert_eq!(state.transcript.selected_cell, 1);
        state.transcript_prev(4);
        assert_eq!(state.transcript.selected_cell, 0);
    }
}
