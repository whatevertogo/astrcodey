use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::{render::wrap::wrap_plain_text, state::ComposerState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerRenderState {
    pub lines: Vec<Line<'static>>,
    pub cursor: Option<(u16, u16)>,
}

pub fn render_composer(
    composer: &ComposerState,
    width: u16,
    height: u16,
    focused: bool,
) -> ComposerRenderState {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    if composer.as_str().is_empty() {
        return ComposerRenderState {
            lines: vec![Line::from("")],
            cursor: focused.then_some((0, 0)),
        };
    }

    let wrapped_all = wrap_plain_text(composer.as_str(), width);
    let prefix = &composer.as_str()[..composer.cursor.min(composer.as_str().len())];
    let wrapped_prefix = wrap_plain_text(prefix, width);
    let cursor_row = wrapped_prefix.len().saturating_sub(1);
    let cursor_col = wrapped_prefix
        .last()
        .map(|line| UnicodeWidthStr::width(line.as_str()) as u16)
        .unwrap_or(0);
    let visible_start = cursor_row.saturating_add(1).saturating_sub(height);
    let visible_lines = wrapped_all
        .iter()
        .skip(visible_start)
        .take(height)
        .cloned()
        .map(Line::from)
        .collect::<Vec<_>>();
    let cursor = focused.then_some((cursor_col, cursor_row.saturating_sub(visible_start) as u16));

    ComposerRenderState {
        lines: if visible_lines.is_empty() {
            vec![Line::from("")]
        } else {
            visible_lines
        },
        cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::render_composer;
    use crate::state::ComposerState;

    #[test]
    fn empty_focused_composer_keeps_blank_canvas() {
        let composer = ComposerState::default();
        let rendered = render_composer(&composer, 24, 2, true);
        assert_eq!(rendered.lines.len(), 1);
        assert!(rendered.lines[0].to_string().is_empty());
        assert_eq!(rendered.cursor, Some((0, 0)));
    }
}
