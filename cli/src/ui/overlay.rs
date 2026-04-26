use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::{
    state::CliState,
    ui::{
        CodexTheme,
        cells::{RenderableCell, TranscriptCellView},
        custom_terminal::Frame,
        materialize_wrapped_lines,
    },
};

pub fn render_browser_overlay(frame: &mut Frame<'_>, state: &CliState, theme: &CodexTheme) {
    let area = centered_rect(frame.area());
    frame.render_widget(Clear, area);

    let title = state
        .conversation
        .active_session_title
        .clone()
        .map(|title| format!("History · {title}"))
        .unwrap_or_else(|| "History".to_string());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let rendered = browser_lines(state, chunks[0].width, theme);
    let content_height = usize::from(chunks[0].height.max(1));
    let scroll = overlay_scroll_offset(
        rendered.lines.len(),
        content_height,
        rendered.selected_line_range,
    );

    frame.render_widget(
        Paragraph::new(rendered.lines.clone()).scroll((scroll as u16, 0)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(browser_footer(state, rendered.total_cells)),
        chunks[1],
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserRenderOutput {
    lines: Vec<ratatui::text::Line<'static>>,
    selected_line_range: Option<(usize, usize)>,
    total_cells: usize,
}

fn browser_lines(state: &CliState, width: u16, theme: &CodexTheme) -> BrowserRenderOutput {
    let width = usize::from(width.max(28));
    let mut lines = Vec::new();
    let mut selected_line_range = None;
    let transcript_cells = state.browser_transcript_cells();

    if let Some(banner) = &state.conversation.banner {
        lines.extend(materialize_wrapped_lines(
            &[
                crate::state::WrappedLine::plain(
                    crate::state::WrappedLineStyle::ErrorText,
                    format!("! {}", banner.error.message),
                ),
                crate::state::WrappedLine::plain(
                    crate::state::WrappedLineStyle::Muted,
                    "  stream 需要重新同步，继续操作前建议等待恢复。".to_string(),
                ),
                crate::state::WrappedLine::plain(
                    crate::state::WrappedLineStyle::Plain,
                    String::new(),
                ),
            ],
            width,
            theme,
        ));
    }

    for (index, cell) in transcript_cells.iter().enumerate() {
        let line_start = lines.len();
        let view = TranscriptCellView {
            selected: state.interaction.browser.selected_cell == index,
            expanded: state.is_cell_expanded(cell.id.as_str()) || cell.expanded,
            thinking: match &cell.kind {
                crate::state::TranscriptCellKind::Thinking { body, status } => {
                    Some(state.thinking_playback.present(
                        &state.thinking_pool,
                        cell.id.as_str(),
                        body.as_str(),
                        *status,
                        state.is_cell_expanded(cell.id.as_str()) || cell.expanded,
                    ))
                },
                _ => None,
            },
        };
        let rendered = cell.render_lines(width, state.shell.capabilities, theme, &view);
        lines.extend(materialize_wrapped_lines(&rendered, width, theme));
        if view.selected {
            selected_line_range = Some((line_start, lines.len().saturating_sub(1)));
        }
    }

    BrowserRenderOutput {
        lines,
        selected_line_range,
        total_cells: transcript_cells.len(),
    }
}

fn overlay_scroll_offset(
    total_lines: usize,
    viewport_height: usize,
    selected_line_range: Option<(usize, usize)>,
) -> usize {
    let max_scroll = total_lines.saturating_sub(viewport_height);
    let Some((selected_start, selected_end)) = selected_line_range else {
        return max_scroll;
    };
    if selected_end < viewport_height {
        0
    } else {
        selected_end
            .saturating_add(1)
            .saturating_sub(viewport_height)
            .min(max_scroll)
            .min(selected_start)
    }
}

fn browser_footer(state: &CliState, total_cells: usize) -> String {
    let unseen = total_cells > state.interaction.browser.last_seen_cell_count
        && state.interaction.browser.selected_cell + 1 < total_cells;
    if unseen {
        "Esc close · ↑↓ browse · Home/End jump · PgUp/PgDn page · End 查看新消息".to_string()
    } else {
        "Esc close · ↑↓ browse · Home/End jump · PgUp/PgDn page · Enter expand".to_string()
    }
}

fn centered_rect(area: Rect) -> Rect {
    let horizontal_margin = area.width.saturating_div(12).max(1);
    let vertical_margin = area.height.saturating_div(10).max(1);
    Rect {
        x: area.x.saturating_add(horizontal_margin),
        y: area.y.saturating_add(vertical_margin),
        width: area
            .width
            .saturating_sub(horizontal_margin.saturating_mul(2)),
        height: area
            .height
            .saturating_sub(vertical_margin.saturating_mul(2)),
    }
}

#[cfg(test)]
mod tests {
    use super::overlay_scroll_offset;

    #[test]
    fn overlay_scroll_defaults_to_bottom_when_nothing_selected() {
        assert_eq!(overlay_scroll_offset(20, 5, None), 15);
    }

    #[test]
    fn overlay_scroll_keeps_selected_range_visible() {
        assert_eq!(overlay_scroll_offset(30, 6, Some((20, 22))), 17);
    }
}
