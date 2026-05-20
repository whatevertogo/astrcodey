//! RenderSpec → Vec<Line> conversion.
//!
//! Pure functions: no dependency on App state or Component trait.
//! Used by transcript, tool_row, and any component that needs to display a RenderSpec.

use astrcode_core::render::{RenderSpec, RenderTone};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::tui::theme::Theme;

/// Convert a `RenderSpec` tree into a flat list of styled `Line`s.
///
/// `prefix` is prepended to every line (e.g. `"  "` for 2-space indent).
/// `width` is the available column count (used for wrapping).
pub fn render_spec_to_lines(
    spec: &RenderSpec,
    prefix: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_spec_inner(spec, &mut lines, width, theme, prefix);
    lines
}

fn render_spec_inner(
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
        RenderSpec::List { items, .. } => {
            for item in items {
                match item {
                    RenderSpec::Text { text, tone: _ } => {
                        push_wrapped_line(lines, prefix, &format!("* {text}"), theme.body, width);
                    },
                    other => {
                        let item_prefix = format!("{prefix}* ");
                        render_spec_inner(other, lines, width, theme, &item_prefix);
                    },
                }
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
        RenderSpec::Progress {
            label,
            status,
            value,
            tone: _,
        } => {
            let mut text = format!("* {label}");
            if let Some(s) = status {
                text.push_str(" · ");
                text.push_str(s);
            }
            if let Some(v) = value {
                text.push_str(&format!(" · {:.0}%", v.clamp(0.0, 1.0) * 100.0));
            }
            push_wrapped_line(lines, prefix, &text, theme.body, width);
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
        RenderSpec::ImageRef { uri, alt, tone: _ } => {
            let caption = alt.as_deref().unwrap_or(uri);
            push_wrapped_line(
                lines,
                prefix,
                &format!("[image: {caption}]"),
                theme.body,
                width,
            );
        },
        RenderSpec::RawAnsiLimited { text, tone: _ } => {
            let safe = strip_ansi_limited(text);
            for line in safe.lines() {
                push_wrapped_line(lines, prefix, line, theme.body, width);
            }
        },
    }
}

// ─── Markdown renderer ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct MarkdownStyles {
    body: Style,
    heading: Style,
    marker: Style,
    code: Style,
}

impl MarkdownStyles {
    pub fn assistant(theme: &Theme, body: Style) -> Self {
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

fn render_markdown_to_lines(
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

// ─── Markdown parsers ─────────────────────────────────────────────────────────

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

// ─── Line helpers ─────────────────────────────────────────────────────────────

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

fn tone_style(tone: &RenderTone, theme: &Theme) -> Style {
    match tone {
        RenderTone::Default => theme.body,
        RenderTone::Muted => theme.dim,
        RenderTone::Accent => theme.assistant_label,
        RenderTone::Success => theme.tool_label,
        RenderTone::Warning => theme.tool_label,
        RenderTone::Error => theme.error_label,
    }
}

fn strip_ansi_limited(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let is_csi = if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            true
        } else {
            ch == '\u{9b}'
        };
        if is_csi {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if ch == '\n' || ch == '\t' || !ch.is_control() {
            output.push(ch);
        }
    }
    output
}

fn text_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
        .sum()
}

// ─── Visual layout engine (for composer) ─────────────────────────────────────

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

// ─── Scrollback entry rendering ───────────────────────────────────────────────

use crate::tui::store::transcript::{Message, MessageRole, ScrollbackEntry};

pub fn scrollback_entry_to_lines(
    entry: &ScrollbackEntry,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match entry {
        ScrollbackEntry::Message(msg) => message_to_lines(msg, width, theme),
        ScrollbackEntry::StreamHeader { role, label } => stream_header_to_lines(role, label, theme),
        ScrollbackEntry::StreamText { role, text } => {
            stream_text_to_lines(role, text, width, theme)
        },
        ScrollbackEntry::BlankLine => vec![Line::from("")],
    }
}

pub fn message_to_lines(msg: &Message, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let content_width = width.max(1) as usize;
    let mut lines = Vec::new();

    match &msg.role {
        MessageRole::User => {
            // User messages: just show the text with a subtle prefix
            lines.push(Line::from(vec![Span::styled(
                format!("❯ {}", msg.label),
                theme.user_label,
            )]));
            let text = msg.body.plain_text();
            if !text.trim().is_empty() {
                for line in visual_lines(text, content_width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled("  ", theme.dim),
                        Span::styled(line, theme.body),
                    ]));
                }
            }
        },
        MessageRole::Assistant => {
            // Assistant: codex-style with ⎿ tree prefix
            if let Some(spec) = msg.body.render_spec() {
                render_spec_inner(spec, &mut lines, content_width, theme, "  ");
            } else {
                let text = msg.body.plain_text();
                if !text.trim().is_empty() {
                    if !msg.is_streaming {
                        let styles = MarkdownStyles::assistant(theme, body_style(&msg.role, theme));
                        render_markdown_to_lines(text, &mut lines, content_width, "  ", styles);
                    } else {
                        for line in visual_lines(text, content_width.saturating_sub(2)) {
                            lines.push(Line::from(vec![
                                Span::styled("  ", theme.dim),
                                Span::raw(line),
                            ]));
                        }
                    }
                }
            }
        },
        MessageRole::Tool => {
            // Codex-style: single line "⎿ Label  result"
            let text = msg.body.plain_text();
            let first_line = text.lines().next().unwrap_or("").trim();
            lines.push(Line::from(vec![
                Span::styled("  ⎿ ", theme.dim),
                Span::styled(msg.label.clone(), theme.tool_label),
                Span::styled("  ", theme.dim),
                Span::styled(first_line.to_string(), theme.dim),
            ]));
        },
        MessageRole::System => {
            lines.push(Line::from(vec![Span::styled(
                format!("  {} {}", "─", msg.label),
                theme.system_label,
            )]));
            let text = msg.body.plain_text();
            if !text.trim().is_empty() {
                for line in visual_lines(text, content_width.saturating_sub(4)) {
                    lines.push(Line::from(vec![
                        Span::styled("    ", theme.dim),
                        Span::styled(line, theme.dim),
                    ]));
                }
            }
        },
        MessageRole::Error => {
            lines.push(Line::from(vec![
                Span::styled("  ✗ ", theme.error_label),
                Span::styled(msg.label.clone(), theme.error_label),
            ]));
            let text = msg.body.plain_text();
            if !text.trim().is_empty() {
                for line in visual_lines(text, content_width.saturating_sub(4)) {
                    lines.push(Line::from(vec![
                        Span::styled("    ", theme.dim),
                        Span::styled(line, theme.error_label),
                    ]));
                }
            }
        },
    }

    if msg.is_streaming {
        lines.push(Line::from(vec![Span::styled("  ⋯", theme.dim)]));
    }
    // Only add blank line separator for User and Assistant messages (not Tool/System — too
    // compact).
    if matches!(msg.role, MessageRole::User | MessageRole::Assistant) {
        lines.push(Line::from(""));
    }
    lines
}

fn push_message_body_lines(
    msg: &Message,
    content_width: usize,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
) {
    if let Some(spec) = msg.body.render_spec() {
        render_spec_inner(spec, lines, content_width, theme, "  ");
        return;
    }
    let text = msg.body.plain_text();
    if text.trim().is_empty() {
        return;
    }
    if msg.role == MessageRole::Assistant && !msg.is_streaming {
        let styles = MarkdownStyles::assistant(theme, body_style(&msg.role, theme));
        render_markdown_to_lines(text, lines, content_width, "  ", styles);
    } else {
        for line in visual_lines(text, content_width) {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::raw(line),
            ]));
        }
    }
}

fn stream_header_to_lines(_role: &MessageRole, _label: &str, _theme: &Theme) -> Vec<Line<'static>> {
    // No separate header for streaming — just start writing lines.
    // Codex style: assistant streaming content appears with indentation only.
    vec![Line::from("")]
}

fn stream_text_to_lines(
    role: &MessageRole,
    text: &str,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let style = body_style(role, theme);
    visual_lines(text.trim_start(), content_width)
        .into_iter()
        .map(|line| {
            Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled(line, style),
            ])
        })
        .collect()
}

fn role_icon_and_style(role: &MessageRole, theme: &Theme) -> (&'static str, Style) {
    match role {
        MessageRole::User => (">", theme.user_label),
        MessageRole::Assistant => ("*", theme.assistant_label),
        MessageRole::Tool => ("+", theme.tool_label),
        MessageRole::System => ("-", theme.system_label),
        MessageRole::Error => ("!", theme.error_label),
    }
}

fn body_style(role: &MessageRole, theme: &Theme) -> Style {
    if *role == MessageRole::Error {
        theme.body.patch(theme.error_label)
    } else {
        theme.body
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    #[test]
    fn render_spec_markdown_uses_block_structure() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "# Title\n\n- first\n2. second\n> quoted\n---\n```rust\nlet x = 1;\n```".into(),
            tone: RenderTone::Default,
        };
        let lines = render_spec_to_lines(&spec, "  ", 48, &theme);
        let texts = line_texts(&lines);
        assert!(texts.iter().any(|l| l == "  Title"));
        assert!(texts.iter().any(|l| l == "  * first"));
        assert!(texts.iter().any(|l| l == "  2. second"));
        assert!(texts.iter().any(|l| l == "  | quoted"));
        assert!(texts.iter().any(|l| l.starts_with("  ---")));
        assert!(texts.iter().any(|l| l == "  code rust"));
        assert!(texts.iter().any(|l| l == "      let x = 1;"));
        assert!(!texts.iter().any(|l| l.contains("# Title")));
        assert!(!texts.iter().any(|l| l.contains("```")));
    }

    #[test]
    fn markdown_tone_is_preserved_for_error_output() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "# Failure\n- bad".into(),
            tone: RenderTone::Error,
        };
        let lines = render_spec_to_lines(&spec, "  ", 48, &theme);
        let failure = lines
            .iter()
            .find(|l| line_text(l) == "  Failure")
            .expect("heading should render");
        assert_eq!(failure.spans[1].style, theme.error_label);
    }

    #[test]
    fn markdown_respects_parent_prefix_when_wrapping() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "- alpha beta gamma delta".into(),
            tone: RenderTone::Default,
        };
        let lines = render_spec_to_lines(&spec, "  | ", 18, &theme);
        let texts = line_texts(&lines);
        assert!(texts[0].starts_with("  | *"));
        assert!(texts[1].starts_with("    "));
    }

    #[test]
    fn unsupported_inline_markdown_stays_plain_text() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "Keep **bold** and `code` literal".into(),
            tone: RenderTone::Default,
        };
        let lines = render_spec_to_lines(&spec, "  ", 80, &theme);
        let texts = line_texts(&lines);
        assert!(
            texts
                .iter()
                .any(|l| l == "  Keep **bold** and `code` literal")
        );
    }
}
