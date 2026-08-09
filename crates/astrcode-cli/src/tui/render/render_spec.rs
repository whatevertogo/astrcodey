//! CLI-local render tree → Vec<Line> conversion, markdown parser, visual layout engine.
//!
//! Pure functions: no dependency on App state, Message structs, or extension points.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::tui::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RenderTone {
    #[default]
    Default,
    Muted,
    Accent,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderKeyValue {
    pub key: String,
    pub value: String,
    pub tone: RenderTone,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderSpec {
    Text {
        text: String,
        tone: RenderTone,
    },
    Markdown {
        text: String,
        tone: RenderTone,
    },
    Box {
        title: Option<String>,
        tone: RenderTone,
        children: Vec<RenderSpec>,
    },
    KeyValue {
        entries: Vec<RenderKeyValue>,
        tone: RenderTone,
    },
    Diff {
        text: String,
        tone: RenderTone,
    },
    Code {
        language: Option<String>,
        text: String,
        tone: RenderTone,
    },
}

impl RenderSpec {
    pub fn plain_text_fallback(&self) -> String {
        match self {
            Self::Text { text, .. }
            | Self::Markdown { text, .. }
            | Self::Diff { text, .. }
            | Self::Code { text, .. } => text.clone(),
            Self::Box {
                title, children, ..
            } => {
                let mut output = title.clone().unwrap_or_default();
                for child in children {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&child.plain_text_fallback());
                }
                output
            },
            Self::KeyValue { entries, .. } => entries
                .iter()
                .map(|entry| format!("{}: {}", entry.key, entry.value))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

pub(crate) fn render_spec_inner(
    spec: &RenderSpec,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: &Theme,
    prefix: &str,
) {
    match spec {
        RenderSpec::Text { text, tone } => {
            push_wrapped_line(lines, prefix, text, tone_style(tone, theme), width);
        },
        RenderSpec::Markdown { text, tone } => {
            let styles = MarkdownStyles::from_tone(tone, theme);
            render_markdown_to_lines(text, lines, width, prefix, styles);
        },
        RenderSpec::Box {
            title,
            children,
            tone: _,
        } => {
            if let Some(title) = title {
                push_wrapped_line(
                    lines,
                    prefix,
                    &format!("* {title}"),
                    theme.assistant_label,
                    width,
                );
            }
            let child_prefix = format!("{prefix}  | ");
            for child in children {
                render_spec_inner(child, lines, width, theme, &child_prefix);
            }
        },
        RenderSpec::KeyValue { entries, tone: _ } => {
            for entry in entries {
                let text = format!("{}: {}", entry.key, entry.value);
                let style = if entry.tone == RenderTone::Default {
                    theme.body
                } else {
                    tone_style(&entry.tone, theme)
                };
                push_wrapped_line(lines, prefix, &text, style, width);
            }
        },
        RenderSpec::Diff { text, tone: _ } => {
            for line in text.lines() {
                let style = match line.chars().next() {
                    Some('+') => tone_style(&RenderTone::Success, theme),
                    Some('-') => tone_style(&RenderTone::Error, theme),
                    _ => theme.body,
                };
                push_wrapped_line(lines, prefix, line, style, width);
            }
        },
        RenderSpec::Code {
            language,
            text,
            tone: _,
        } => {
            if let Some(lang) = language {
                push_wrapped_line(lines, prefix, &format!("```{lang}"), theme.dim, width);
            }
            for line in text.lines() {
                push_wrapped_line(lines, prefix, line, theme.body, width);
            }
        },
    }
}

// ─── Markdown renderer ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct MarkdownStyles {
    body: Style,
    heading: Style,
    marker: Style,
    code: Style,
}

impl MarkdownStyles {
    pub(crate) fn assistant(theme: &Theme, body: Style) -> Self {
        Self {
            body,
            heading: theme.assistant_label,
            marker: theme.dim,
            code: body,
        }
    }

    fn from_tone(tone: &RenderTone, theme: &Theme) -> Self {
        let body = tone_style(tone, theme);
        let (heading, marker) = if *tone == RenderTone::Default {
            (theme.assistant_label, theme.dim)
        } else {
            (body, body)
        };
        Self {
            body,
            heading,
            marker,
            code: body,
        }
    }
}

pub(crate) fn render_markdown_to_lines(
    text: &str,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    prefix: &str,
    styles: MarkdownStyles,
) {
    let mut in_code = false;
    let mut emitted_any = false;
    let mut pending_blank = false;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        if let Some(language) = parse_code_fence(trimmed) {
            if in_code {
                in_code = false;
            } else {
                in_code = true;
                if !language.is_empty() {
                    push_pending_blank(lines, &mut pending_blank);
                    push_wrapped_line_with_prefix_style(
                        lines,
                        prefix,
                        styles.marker,
                        &format!("code {language}"),
                        styles.marker,
                        width,
                    );
                    emitted_any = true;
                }
            }
            continue;
        }
        if in_code {
            push_pending_blank(lines, &mut pending_blank);
            push_code_line(lines, prefix, line, styles.code, width);
            emitted_any = true;
            continue;
        }
        if trimmed.is_empty() {
            pending_blank = emitted_any;
            continue;
        }
        push_pending_blank(lines, &mut pending_blank);

        if let Some(heading) = parse_atx_heading(trimmed) {
            push_wrapped_line_with_prefix_style(
                lines,
                prefix,
                styles.marker,
                heading,
                styles.heading,
                width,
            );
        } else if is_horizontal_rule(trimmed) {
            push_separator_line(lines, prefix, styles.marker, width);
        } else if let Some(item) = parse_unordered_list(trimmed) {
            push_wrapped_line_with_prefix_style(
                lines,
                prefix,
                styles.marker,
                &format!("* {item}"),
                styles.body,
                width,
            );
        } else if let Some((marker, item)) = parse_ordered_list(trimmed) {
            push_wrapped_line_with_prefix_style(
                lines,
                prefix,
                styles.marker,
                &format!("{marker} {item}"),
                styles.body,
                width,
            );
        } else if let Some(quote) = parse_block_quote(trimmed) {
            push_wrapped_line_with_prefix_style(
                lines,
                prefix,
                styles.marker,
                &format!("| {quote}"),
                styles.body,
                width,
            );
        } else {
            push_wrapped_line_with_prefix_style(
                lines,
                prefix,
                styles.marker,
                trimmed,
                styles.body,
                width,
            );
        }
        emitted_any = true;
    }
}

// ─── Markdown parsers ─────────────────────────────────────────────────────

fn parse_code_fence(line: &str) -> Option<&str> {
    line.strip_prefix("```").map(str::trim)
}

fn parse_atx_heading(line: &str) -> Option<&str> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let heading = rest.trim();
    if heading.is_empty() {
        return None;
    }
    Some(trim_trailing_heading_marks(heading))
}

fn trim_trailing_heading_marks(text: &str) -> &str {
    let trimmed = text.trim_end();
    let without_marks = trimmed.trim_end_matches('#').trim_end();
    if without_marks.is_empty() {
        trimmed
    } else {
        without_marks
    }
}

fn parse_unordered_list(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|m| line.strip_prefix(m).map(str::trim_start))
}

fn parse_ordered_list(line: &str) -> Option<(&str, &str)> {
    let digit_end = line
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(i, ch)| i + ch.len_utf8())
        .last()?;
    let marker_end = digit_end + 1;
    if line[digit_end..].chars().next()? != '.' {
        return None;
    }
    let rest = &line[marker_end..];
    if rest.is_empty() || !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    Some((&line[..marker_end], rest.trim_start()))
}

fn parse_block_quote(line: &str) -> Option<&str> {
    line.strip_prefix('>').map(str::trim_start)
}

fn is_horizontal_rule(line: &str) -> bool {
    let compact = line.split_whitespace().collect::<String>();
    if compact.chars().count() < 3 {
        return false;
    }
    let Some(first) = compact.chars().next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && compact.chars().all(|ch| ch == first)
}

// ─── Line helpers ─────────────────────────────────────────────────────────

fn push_pending_blank(lines: &mut Vec<Line<'static>>, pending_blank: &mut bool) {
    if *pending_blank {
        lines.push(Line::from(""));
    }
    *pending_blank = false;
}

fn push_code_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    let code_prefix = format!("{prefix}    ");
    if text.is_empty() {
        lines.push(Line::from(Span::styled(code_prefix, style)));
    } else {
        push_wrapped_line(lines, &code_prefix, text, style, width);
    }
}

fn push_separator_line(lines: &mut Vec<Line<'static>>, prefix: &str, style: Style, width: usize) {
    let prefix_width = text_width(prefix);
    let sep_width = width.saturating_sub(prefix_width).clamp(3, 40);
    push_wrapped_line_with_prefix_style(lines, prefix, style, &"-".repeat(sep_width), style, width);
}

fn push_wrapped_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    push_wrapped_line_with_prefix_style(lines, prefix, style, text, style, width);
}

fn push_wrapped_line_with_prefix_style(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    text: &str,
    style: Style,
    width: usize,
) {
    let prefix_width = text_width(prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let wrapped = visual_lines(text, content_width);
    if wrapped.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::styled("…", style),
        ]));
        return;
    }
    for (i, line) in wrapped.iter().enumerate() {
        let p = if i == 0 {
            prefix.to_string()
        } else {
            " ".repeat(prefix_width)
        };
        lines.push(Line::from(vec![
            Span::styled(p, prefix_style),
            Span::styled(line.clone(), style),
        ]));
    }
}

pub fn visual_lines(text: &str, width: usize) -> Vec<String> {
    layout_visual_text(text, width, None).lines
}

pub(crate) fn tone_style(tone: &RenderTone, theme: &Theme) -> Style {
    match tone {
        RenderTone::Default => theme.body,
        RenderTone::Muted => theme.dim,
        RenderTone::Accent => theme.assistant_label,
        RenderTone::Success => theme.tool_label,
        RenderTone::Error => theme.error_label,
    }
}

fn text_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
        .sum()
}

// ─── Visual layout engine (for composer) ─────────────────────────────────

#[derive(Debug, Default)]
pub struct VisualLayout {
    pub lines: Vec<String>,
    pub cursor_row: Option<usize>,
    pub cursor_column: Option<usize>,
}

pub fn layout_visual_text(text: &str, width: usize, cursor: Option<usize>) -> VisualLayout {
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
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
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
