use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{
    BottomPaneState,
    model::{BottomPaneMode, composer_height},
};
use crate::{
    state::{CliState, PaneFocus},
    ui::{CodexTheme, ThemePalette, composer::render_composer, custom_terminal::Frame},
};

pub fn render_bottom_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &CliState,
    pane: &BottomPaneState,
    theme: &CodexTheme,
) {
    let popup_height = pane.palette_height();
    let composer_height = composer_height(area.height, pane.composer_line_count);

    let body_height = area
        .height
        .saturating_sub(popup_height)
        .saturating_sub(composer_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_height),
            Constraint::Length(popup_height),
            Constraint::Length(composer_height),
        ])
        .split(area);

    render_body(frame, chunks[0], pane, theme);
    render_palette(frame, chunks[1], pane, theme);
    render_composer_area(frame, chunks[2], state, theme);
}

fn render_body(frame: &mut Frame<'_>, area: Rect, pane: &BottomPaneState, theme: &CodexTheme) {
    match &pane.mode {
        BottomPaneMode::EmptySessionMinimal { welcome_lines } => {
            if area.height == 0 {
                return;
            }
            let card_height = (welcome_lines.len() as u16 + 2).min(area.height);
            let card_area = Rect::new(area.x, area.y, area.width.min(48), card_height);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(theme.menu_block_style());
            let inner = block.inner(card_area);
            frame.render_widget(block, card_area);
            frame.render_widget(Paragraph::new(welcome_lines.clone()), inner);
        },
        BottomPaneMode::ActiveSession {
            status_line,
            detail_lines,
            preview_lines,
        } => {
            let status_height = status_line.is_some() as u16;
            let detail_height = detail_lines.len().min(2) as u16;
            let preview_height = preview_lines.len().min(3) as u16;
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(status_height),
                    Constraint::Length(detail_height),
                    Constraint::Length(preview_height),
                    Constraint::Min(0),
                ])
                .split(area);

            if let Some(line) = status_line {
                frame.render_widget(Paragraph::new(vec![line.clone()]), chunks[0]);
            }
            if detail_height > 0 {
                frame.render_widget(
                    Paragraph::new(
                        detail_lines
                            .iter()
                            .take(detail_height as usize)
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                    chunks[1],
                );
            }
            if preview_height > 0 {
                let visible_preview = preview_tail(preview_lines, preview_height as usize);
                frame.render_widget(Paragraph::new(visible_preview), chunks[2]);
            }
        },
    }
}

fn render_palette(frame: &mut Frame<'_>, area: Rect, pane: &BottomPaneState, theme: &CodexTheme) {
    if area.height == 0 {
        return;
    }
    if let Some(title) = &pane.palette_title {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.menu_block_style())
            .title(title.clone());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(pane.palette_lines.clone()), inner);
    }
}

fn render_composer_area(frame: &mut Frame<'_>, area: Rect, state: &CliState, theme: &CodexTheme) {
    if area.height == 0 {
        return;
    }
    let focused = matches!(
        state.interaction.pane_focus,
        PaneFocus::Composer | PaneFocus::Palette
    );
    let composer = render_composer(
        &state.interaction.composer,
        area.width.saturating_sub(2),
        area.height,
        focused,
    );
    let prompt = theme.glyph("›", ">");
    let composer_lines = composer
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                ratatui::text::Line::from(format!("{prompt} {}", line))
            } else {
                ratatui::text::Line::from(format!("  {}", line))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(composer_lines), area);
    if let Some((cursor_x, cursor_y)) = composer.cursor {
        frame.set_cursor_position((area.x + cursor_x + 2, area.y + cursor_y));
    }
}

fn preview_tail(
    lines: &[ratatui::text::Line<'static>],
    visible_lines: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let skip = lines.len().saturating_sub(visible_lines);
    lines.iter().skip(skip).cloned().collect()
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::preview_tail;

    #[test]
    fn preview_tail_prefers_latest_lines() {
        let lines = vec![
            Line::from("line-1"),
            Line::from("line-2"),
            Line::from("line-3"),
            Line::from("line-4"),
        ];

        let visible = preview_tail(&lines, 2)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert_eq!(visible, vec!["line-3".to_string(), "line-4".to_string()]);
    }
}
