pub mod cells;
pub mod composer;
pub mod custom_terminal;
pub mod insert_history;
mod markdown;
pub mod overlay;
mod palette;
mod text;
mod theme;

pub use palette::palette_lines;
use ratatui::text::{Line, Span};
pub use text::truncate_to_width;
pub use theme::{CodexTheme, ThemePalette};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    render::wrap::wrap_plain_text,
    state::{WrappedLine, WrappedLineRewrapPolicy},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryLine {
    pub line: Line<'static>,
    pub rewrap_policy: WrappedLineRewrapPolicy,
}

pub fn line_to_ratatui(line: &WrappedLine, theme: &CodexTheme) -> Line<'static> {
    let base = theme.line_style(line.style);
    let spans = if line.spans.is_empty() {
        vec![Span::styled(String::new(), base)]
    } else {
        line.spans
            .iter()
            .map(|span| {
                let style = span
                    .style
                    .map(|style| base.patch(theme.span_style(style)))
                    .unwrap_or(base);
                Span::styled(span.content.clone(), style)
            })
            .collect::<Vec<_>>()
    };
    Line::from(spans).style(base)
}

pub fn history_line_to_ratatui(line: WrappedLine, theme: &CodexTheme) -> HistoryLine {
    HistoryLine {
        rewrap_policy: line.rewrap_policy,
        line: line_to_ratatui(&line, theme),
    }
}

pub(crate) fn materialize_wrapped_line(
    line: &WrappedLine,
    width: usize,
    theme: &CodexTheme,
) -> Vec<Line<'static>> {
    let history_line = history_line_to_ratatui(line.clone(), theme);
    materialize_history_line(&history_line, width)
}

pub(crate) fn materialize_wrapped_lines(
    lines: &[WrappedLine],
    width: usize,
    theme: &CodexTheme,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .flat_map(|line| materialize_wrapped_line(line, width, theme))
        .collect()
}

pub(crate) fn materialize_history_line(line: &HistoryLine, width: usize) -> Vec<Line<'static>> {
    match line.rewrap_policy {
        WrappedLineRewrapPolicy::Reflow => wrap_reflow_line(&line.line, width),
        WrappedLineRewrapPolicy::PreserveAndCrop => vec![crop_line(&line.line, width.max(1))],
    }
}

fn wrap_reflow_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let content = line.to_string();
    let wrapped = wrap_plain_text(content.as_str(), width.max(1));
    if line.spans.is_empty() {
        return wrapped
            .into_iter()
            .map(|item| Line::from(item).style(line.style))
            .collect();
    }

    let mut cursor = StyledGraphemeCursor::new(&line.spans);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            if index > 0 && !starts_with_whitespace(item.as_str()) {
                cursor.skip_boundary_whitespace();
            }
            let spans = cursor.consume_text(item.as_str());
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn crop_line(line: &Line<'static>, width: usize) -> Line<'static> {
    let width = width.max(1);
    if line.width() <= width {
        return line.clone();
    }

    let ellipsis = if width == 1 { "" } else { "…" };
    let budget = width.saturating_sub(display_width(ellipsis));
    let mut visible = Vec::new();
    let mut used = 0usize;

    for span in &line.spans {
        let cropped = crop_span_to_width(span, budget.saturating_sub(used));
        if cropped.content.is_empty() {
            break;
        }
        used += display_width(cropped.content.as_ref());
        visible.push(cropped);
        if used >= budget {
            break;
        }
    }

    if !ellipsis.is_empty() {
        if let Some(last) = visible.last_mut() {
            last.content = format!("{}{}", last.content, ellipsis).into();
        } else {
            visible.push(Span::raw(ellipsis.to_string()));
        }
    }

    Line::from(visible).style(line.style)
}

fn crop_span_to_width(span: &Span<'static>, width: usize) -> Span<'static> {
    if width == 0 {
        return Span::styled(String::new(), span.style);
    }

    let mut content = String::new();
    let mut used = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(span.content.as_ref(), true) {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        content.push_str(grapheme);
        used += grapheme_width;
    }
    Span::styled(content, span.style)
}

fn display_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(unicode_width::UnicodeWidthStr::width)
        .sum()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledGrapheme {
    style: ratatui::style::Style,
    text: String,
}

struct StyledGraphemeCursor {
    graphemes: Vec<StyledGrapheme>,
    index: usize,
}

impl StyledGraphemeCursor {
    fn new(spans: &[Span<'static>]) -> Self {
        let graphemes = spans
            .iter()
            .flat_map(|span| {
                UnicodeSegmentation::graphemes(span.content.as_ref(), true).map(|grapheme| {
                    StyledGrapheme {
                        style: span.style,
                        text: grapheme.to_string(),
                    }
                })
            })
            .collect();
        Self {
            graphemes,
            index: 0,
        }
    }

    fn consume_text(&mut self, text: &str) -> Vec<Span<'static>> {
        if text.is_empty() {
            return Vec::new();
        }

        let mut spans = Vec::new();
        let mut current_style = None;
        let mut current_text = String::new();

        for grapheme in UnicodeSegmentation::graphemes(text, true) {
            let Some(next) = self.next_matching(grapheme) else {
                return vec![Span::raw(text.to_string())];
            };
            match current_style {
                Some(style) if style == next.style => current_text.push_str(next.text.as_str()),
                Some(style) => {
                    spans.push(Span::styled(std::mem::take(&mut current_text), style));
                    current_text.push_str(next.text.as_str());
                    current_style = Some(next.style);
                },
                None => {
                    current_text.push_str(next.text.as_str());
                    current_style = Some(next.style);
                },
            }
        }

        if let Some(style) = current_style {
            spans.push(Span::styled(current_text, style));
        }

        spans
    }

    fn skip_boundary_whitespace(&mut self) {
        while self
            .graphemes
            .get(self.index)
            .is_some_and(|grapheme| grapheme.text.chars().all(char::is_whitespace))
        {
            self.index += 1;
        }
    }

    fn next_matching(&mut self, expected: &str) -> Option<StyledGrapheme> {
        let grapheme = self.graphemes.get(self.index)?.clone();
        if grapheme.text == expected {
            self.index += 1;
            Some(grapheme)
        } else {
            None
        }
    }
}

fn starts_with_whitespace(text: &str) -> bool {
    text.chars().next().is_some_and(char::is_whitespace)
}
