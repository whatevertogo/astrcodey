//! Codex-inspired render pass: transcript viewport on top, focused composer on bottom.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use super::{
    slash,
    state::{Focus, MessageRole, TuiState},
    theme::Theme,
};

pub fn render(state: &TuiState, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    let composer_height = composer_height(state, area.width)
        .min(area.height.saturating_sub(2))
        .max(3);
    let status_height = if state.is_streaming || state.error.is_some() {
        1
    } else {
        0
    };
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_transcript(state, frame, layout[0], theme);
    if status_height > 0 {
        render_status(state, frame, layout[1], theme);
    }
    render_composer(state, frame, layout[2], theme);
    render_footer(state, frame, layout[3], theme);

    if state.show_slash_palette {
        render_slash_palette(state, frame, area, theme);
    }
}

fn render_transcript(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border);
    let inner = block.inner(area);
    let lines = build_transcript_lines(state, inner.width, theme);
    let visible = clip_to_bottom(lines, inner.height as usize);
    let paragraph = Paragraph::new(Text::from(visible)).block(block);
    frame.render_widget(paragraph, area);
}

fn render_status(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let line = if let Some(error) = &state.error {
        Line::from(vec![
            Span::styled("error ", theme.error_label),
            Span::styled(error.clone(), theme.body),
        ])
    } else if state.is_streaming {
        Line::from(vec![
            Span::styled("working ", theme.status_busy),
            Span::styled(state.status.clone(), theme.body),
        ])
    } else {
        Line::from(Span::styled(state.status.clone(), theme.status))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_composer(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let active = state.focus == Focus::Input || state.focus == Focus::SlashPalette;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if active {
            theme.border_active
        } else {
            theme.border
        })
        .title(" Composer ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_width = inner.width.max(1);
    let (lines, cursor) = composer_lines_and_cursor(state, content_width);
    let styled_lines: Vec<Line> = if state.input.is_empty() {
        vec![Line::from(vec![
            Span::styled("› ", theme.assistant_label),
            Span::styled(
                "Ask astrcode to inspect, edit, or explain…",
                theme.composer_placeholder,
            ),
        ])]
    } else {
        lines
            .into_iter()
            .enumerate()
            .map(|(idx, line)| {
                let prefix = if idx == 0 { "› " } else { "  " };
                Line::from(vec![
                    Span::styled(prefix, theme.assistant_label),
                    Span::styled(line, theme.composer),
                ])
            })
            .collect()
    };

    frame.render_widget(Paragraph::new(Text::from(styled_lines)), inner);

    if active {
        let cursor_x = inner.x + cursor.0.min(inner.width.saturating_sub(1));
        let cursor_y = inner.y + cursor.1.min(inner.height.saturating_sub(1));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_footer(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let session = state
        .active_session_id
        .as_deref()
        .map(short_id)
        .unwrap_or("none");
    let model = if state.model_name.is_empty() {
        "model: pending".to_string()
    } else {
        format!("model: {}", state.model_name)
    };
    let sessions = if state.available_sessions.is_empty() {
        String::new()
    } else {
        format!("  ·  sessions: {}", state.available_sessions.len())
    };
    let line = format!(
        "{}  ·  session: {}{}  ·  Enter send  ·  Shift+Enter newline  ·  Ctrl+C quit",
        model, session, sessions
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(line, theme.footer))),
        area,
    );
}

fn render_slash_palette(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let commands = slash::filtered(&state.slash_filter);
    if commands.is_empty() {
        return;
    }

    let height = commands.len().min(6) as u16 + 2;
    let popup = centered_rect(area, 70, height);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let lines: Vec<Line> = commands
        .iter()
        .enumerate()
        .map(|(idx, command)| {
            let selected = idx == state.slash_selected.min(commands.len().saturating_sub(1));
            let label_style = if selected {
                theme.popup_selected
            } else {
                theme.assistant_label
            };
            let desc_style = if selected { theme.body } else { theme.dim };
            Line::from(vec![
                Span::styled(format!("{:<16}", command.usage), label_style),
                Span::styled(command.description, desc_style),
            ])
        })
        .collect();

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.popup_border)
            .title(" Slash Commands "),
        popup,
    );
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn build_transcript_lines(state: &TuiState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let mut lines = Vec::new();

    if state.messages.is_empty() {
        return vec![
            Line::from(Span::styled("Astrcode", theme.assistant_label)),
            Line::from(Span::styled(
                "  Start typing below. This view now stays fully inside the TUI instead of \
                 spilling into terminal scrollback.",
                theme.dim,
            )),
        ];
    }

    for message in state.messages.iter().rev().take(120).rev() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }

        let label_style = message_label_style(&message.role, theme);
        lines.push(Line::from(Span::styled(message.label.clone(), label_style)));

        let body_style = if message.role == MessageRole::Error {
            theme.body.patch(theme.error_label)
        } else {
            theme.body
        };

        let wrapped = visual_lines(&message.content, content_width);
        if wrapped.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled("…", theme.dim),
            ]));
        } else {
            for line in wrapped {
                lines.push(Line::from(vec![
                    Span::styled("  ", theme.dim),
                    Span::styled(line, body_style),
                ]));
            }
        }

        if message.is_streaming {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled("streaming…", theme.dim),
            ]));
        }
    }

    lines
}

fn message_label_style(role: &MessageRole, theme: &Theme) -> Style {
    match role {
        MessageRole::User => theme.user_label,
        MessageRole::Assistant => theme.assistant_label,
        MessageRole::Tool => theme.tool_label,
        MessageRole::System => theme.system_label,
        MessageRole::Error => theme.error_label,
    }
}

fn composer_lines_and_cursor(state: &TuiState, width: u16) -> (Vec<String>, (u16, u16)) {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let layout = layout_visual_text(&state.input, content_width, Some(state.input_cursor));
    let lines = layout.lines;
    let cursor = (
        2 + layout.cursor_column.unwrap_or(0) as u16,
        layout.cursor_row.unwrap_or(0) as u16,
    );

    if lines.is_empty() {
        (Vec::new(), (2, 0))
    } else {
        (lines, cursor)
    }
}

fn visual_lines(text: &str, width: usize) -> Vec<String> {
    layout_visual_text(text, width, None).lines
}

fn clip_to_bottom(lines: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    if height == 0 || lines.len() <= height {
        return lines;
    }
    lines[lines.len() - height..].to_vec()
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = ((area.width as u32 * percent_x as u32) / 100) as u16;
    let popup_width = width.max(24).min(area.width);
    let popup_height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    }
}

fn composer_height(state: &TuiState, width: u16) -> u16 {
    let content_width = width.saturating_sub(4).max(1) as usize;
    let lines = visual_lines(&state.input, content_width).len().max(1) as u16;
    (lines + 2).min(8)
}

fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

#[derive(Debug, Default)]
struct VisualLayout {
    lines: Vec<String>,
    cursor_row: Option<usize>,
    cursor_column: Option<usize>,
}

fn layout_visual_text(text: &str, width: usize, cursor: Option<usize>) -> VisualLayout {
    if width == 0 {
        return VisualLayout {
            lines: vec![],
            cursor_row: Some(0),
            cursor_column: Some(0),
        };
    }

    let mut layout = VisualLayout::default();
    let mut current_line = String::new();
    let mut current_width = 0usize;
    let mut current_row = 0usize;
    let mut consumed_chars = 0usize;

    if cursor == Some(0) {
        layout.cursor_row = Some(0);
        layout.cursor_column = Some(0);
    }

    for ch in text.chars() {
        if cursor == Some(consumed_chars) {
            layout.cursor_row = Some(current_row);
            layout.cursor_column = Some(current_width);
        }

        if ch == '\n' {
            layout.lines.push(std::mem::take(&mut current_line));
            current_width = 0;
            current_row += 1;
            consumed_chars += 1;

            if cursor == Some(consumed_chars) {
                layout.cursor_row = Some(current_row);
                layout.cursor_column = Some(0);
            }
            continue;
        }

        let ch_width = display_width(ch);
        if current_width + ch_width > width && !current_line.is_empty() {
            layout.lines.push(std::mem::take(&mut current_line));
            current_width = 0;
            current_row += 1;

            if cursor == Some(consumed_chars) {
                layout.cursor_row = Some(current_row);
                layout.cursor_column = Some(0);
            }
        }

        current_line.push(ch);
        current_width += ch_width;
        consumed_chars += 1;

        if cursor == Some(consumed_chars) {
            layout.cursor_row = Some(current_row);
            layout.cursor_column = Some(current_width);
        }
    }

    if cursor == Some(consumed_chars) {
        layout.cursor_row = Some(current_row);
        layout.cursor_column = Some(current_width);
    }

    layout.lines.push(current_line);
    layout
}

fn display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::TuiState;

    #[test]
    fn wraps_cjk_by_terminal_width() {
        assert_eq!(visual_lines("你好世界", 4), vec!["你好", "世界"]);
    }

    #[test]
    fn cursor_uses_display_width_for_cjk() {
        let mut state = TuiState::new();
        state.input = "你好".into();
        state.input_cursor = 2;

        let (lines, cursor) = composer_lines_and_cursor(&state, 6);
        assert_eq!(lines, vec!["你好"]);
        assert_eq!(cursor, (6, 0));
    }

    #[test]
    fn composer_height_counts_soft_wraps() {
        let mut state = TuiState::new();
        state.input = "你好世界".into();

        assert_eq!(composer_height(&state, 8), 4);
    }
}
