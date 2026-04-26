use ratatui::text::Line;

use crate::{
    chat::ChatSurfaceFrame,
    state::{CliState, PaletteState, WrappedLine, WrappedLineStyle},
    ui::{CodexTheme, line_to_ratatui, materialize_wrapped_lines, palette_lines},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BottomPaneMode {
    EmptySessionMinimal {
        welcome_lines: Vec<Line<'static>>,
    },
    ActiveSession {
        status_line: Option<Line<'static>>,
        detail_lines: Vec<Line<'static>>,
        preview_lines: Vec<Line<'static>>,
    },
}

impl Default for BottomPaneMode {
    fn default() -> Self {
        Self::ActiveSession {
            status_line: None,
            detail_lines: Vec::new(),
            preview_lines: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BottomPaneState {
    pub mode: BottomPaneMode,
    pub composer_line_count: usize,
    pub palette_title: Option<String>,
    pub palette_lines: Vec<Line<'static>>,
}

impl BottomPaneState {
    pub fn from_cli(
        state: &CliState,
        chat: &ChatSurfaceFrame,
        theme: &CodexTheme,
        width: u16,
    ) -> Self {
        let palette_title = palette_title(&state.interaction.palette);
        let palette_lines = if palette_title.is_some() {
            palette_lines(
                &state.interaction.palette,
                usize::from(width.saturating_sub(4).max(1)),
                theme,
            )
            .into_iter()
            .map(|line| line_to_ratatui(&line, theme))
            .collect()
        } else {
            Vec::new()
        };

        Self {
            mode: if should_show_empty_session_minimal(state, chat) {
                BottomPaneMode::EmptySessionMinimal {
                    welcome_lines: build_welcome_lines(state),
                }
            } else {
                BottomPaneMode::ActiveSession {
                    status_line: active_status_line(state, chat)
                        .map(|line| line_to_ratatui(&line, theme)),
                    detail_lines: materialize_wrapped_lines(
                        &chat.detail_lines,
                        usize::from(width.max(1)),
                        theme,
                    ),
                    preview_lines: materialize_wrapped_lines(
                        &chat.preview_lines,
                        usize::from(width.max(1)),
                        theme,
                    ),
                }
            },
            composer_line_count: state.interaction.composer.line_count(),
            palette_title,
            palette_lines,
        }
    }

    pub fn desired_height(&self, total_height: u16) -> u16 {
        let palette_height = self.palette_height();
        let composer_height = composer_height(total_height, self.composer_line_count);
        let content_height = match &self.mode {
            BottomPaneMode::EmptySessionMinimal { welcome_lines } => {
                (welcome_lines.len() as u16 + 2).clamp(5, 6)
            },
            BottomPaneMode::ActiveSession {
                status_line,
                detail_lines,
                preview_lines,
            } => (status_line.is_some() as u16)
                .saturating_add(detail_lines.len().min(2) as u16)
                .saturating_add(preview_lines.len().min(3) as u16),
        };

        content_height
            .saturating_add(palette_height)
            .saturating_add(composer_height)
            .clamp(bottom_pane_min_height(total_height), total_height.max(1))
    }

    pub fn palette_height(&self) -> u16 {
        if self.palette_title.is_some() {
            (self.palette_lines.len() as u16 + 2).clamp(3, 7)
        } else {
            0
        }
    }
}

pub fn composer_height(total_height: u16, line_count: usize) -> u16 {
    let preferred = line_count.clamp(1, 3) as u16;
    if total_height <= 4 { 1 } else { preferred }
}

fn bottom_pane_min_height(total_height: u16) -> u16 {
    if total_height <= 8 { 3 } else { 4 }
}

fn should_show_empty_session_minimal(state: &CliState, chat: &ChatSurfaceFrame) -> bool {
    state.conversation.transcript.is_empty()
        && chat.status_line.is_none()
        && chat.detail_lines.is_empty()
        && chat.preview_lines.is_empty()
        && !state.interaction.browser.open
}

fn build_welcome_lines(state: &CliState) -> Vec<Line<'static>> {
    let model = state
        .shell
        .current_model
        .as_ref()
        .map(|model| model.model.clone())
        .unwrap_or_else(|| "loading".to_string());
    let directory = state
        .shell
        .working_dir
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "~".to_string());

    vec![
        Line::from(">_ Astrcode CLI"),
        Line::from(format!("model: {model}")),
        Line::from(format!("directory: {directory}")),
    ]
}

fn active_status_line(state: &CliState, chat: &ChatSurfaceFrame) -> Option<WrappedLine> {
    if state.interaction.status.is_error {
        return Some(WrappedLine::plain(
            WrappedLineStyle::Plain,
            format!("• {}", state.interaction.status.message),
        ));
    }
    let trimmed = state.interaction.status.message.trim();
    if !trimmed.is_empty() && trimmed != "ready" {
        return Some(WrappedLine::plain(
            WrappedLineStyle::Plain,
            format!("• {trimmed}"),
        ));
    }
    chat.status_line.clone()
}

fn palette_title(palette: &PaletteState) -> Option<String> {
    match palette {
        PaletteState::Slash(_) => Some("/ commands".to_string()),
        PaletteState::Resume(_) => Some("/resume".to_string()),
        PaletteState::Model(_) => Some("/model".to_string()),
        PaletteState::Closed => None,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_client::CurrentModelInfoDto;

    use super::{
        BottomPaneMode, BottomPaneState, composer_height, should_show_empty_session_minimal,
    };
    use crate::{
        capability::{ColorLevel, GlyphMode, TerminalCapabilities},
        chat::ChatSurfaceFrame,
        state::CliState,
        ui::CodexTheme,
    };

    fn capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::Ansi16,
            glyphs: GlyphMode::Unicode,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn empty_session_uses_minimal_mode() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.shell.current_model = Some(CurrentModelInfoDto {
            profile_name: "default".to_string(),
            model: "glm-5.1".to_string(),
            provider_kind: "glm".to_string(),
        });
        let chat = ChatSurfaceFrame::default();
        assert!(should_show_empty_session_minimal(&state, &chat));

        let theme = CodexTheme::new(state.shell.capabilities);
        let pane = BottomPaneState::from_cli(&state, &chat, &theme, 80);
        match &pane.mode {
            BottomPaneMode::EmptySessionMinimal { welcome_lines } => {
                assert_eq!(welcome_lines.len(), 3);
                assert!(welcome_lines[0].to_string().contains("Astrcode CLI"));
            },
            BottomPaneMode::ActiveSession { .. } => panic!("empty session should use minimal mode"),
        }
        assert!((6..=8).contains(&pane.desired_height(24)));
    }

    #[test]
    fn composer_height_stays_single_line_by_default() {
        assert_eq!(composer_height(24, 1), 1);
        assert_eq!(composer_height(24, 4), 3);
    }
}
