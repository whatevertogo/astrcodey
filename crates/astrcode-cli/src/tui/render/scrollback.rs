//! Scrollback entry → terminal Lines conversion.
//!
//! Maps `ScrollbackEntry` and `Message` structs to styled `Line`s for
//! terminal scrollback rendering. Depends on the pure rendering functions
//! in `render_spec` and the theme.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::render_spec::{
    MarkdownStyles, render_markdown_to_lines, render_spec_inner, visual_lines,
};
use crate::tui::{
    ext::message::MessageRendererRegistry,
    store::transcript::{Message, MessageRole, ScrollbackEntry},
    theme::Theme,
};

// ─── Role style ───────────────────────────────────────────────────────────

/// Visual style for each `MessageRole` in the scrollback.
///
/// Centralises all per-role rendering decisions: icon character,
/// label style, body indentation prefix, text style, and whether
/// a trailing blank separator is emitted after the message.
struct RoleStyle {
    icon: &'static str,
    label_style: fn(&Theme) -> Style,
    body_prefix: &'static str,
    body_style: fn(&Theme) -> Style,
    separator: bool,
}

fn role_style(role: &MessageRole) -> RoleStyle {
    match role {
        MessageRole::User => RoleStyle {
            icon: "❯",
            label_style: |t| t.user_label.patch(t.user_bg),
            body_prefix: "  ",
            body_style: |t| t.body.patch(t.user_bg),
            separator: true,
        },
        MessageRole::Assistant => RoleStyle {
            icon: "*",
            label_style: |_| Style::default(), // Assistant has no header line
            body_prefix: "  ",
            body_style: |t| t.body,
            separator: true,
        },
        MessageRole::Tool => RoleStyle {
            icon: "⎿",
            label_style: |t| t.tool_label,
            body_prefix: "  ",
            body_style: |t| t.dim,
            separator: false,
        },
        MessageRole::System => RoleStyle {
            icon: "─",
            label_style: |t| t.system_label,
            body_prefix: "    ",
            body_style: |t| t.dim,
            separator: false,
        },
        MessageRole::Error => RoleStyle {
            icon: "✗",
            label_style: |t| t.error_label,
            body_prefix: "    ",
            body_style: |t| t.error_label,
            separator: false,
        },
    }
}

// ─── Header rendering ─────────────────────────────────────────────────────

fn render_assistant_header(label: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("  * ", theme.dim),
        Span::styled(label.to_string(), theme.assistant_label),
    ])
}

fn render_header(style: &RoleStyle, msg: &Message, theme: &Theme) -> Vec<Line<'static>> {
    let label = &msg.label;
    match &msg.role {
        MessageRole::Assistant => {
            vec![render_assistant_header(label, theme)]
        },
        MessageRole::Tool => {
            // Tool with RenderSpec: use a box-header style (same as Assistant).
            // Tool without RenderSpec: compact codex one-liner.
            // Handled separately in render_message because plain Tool is
            // collapsed to a single-line summary.
            if msg.body.render_spec().is_some() {
                vec![render_assistant_header(label, theme)]
            } else {
                vec![]
            }
        },
        _ => {
            let icon = style.icon;
            vec![Line::from(vec![
                Span::styled(format!("  {icon} "), theme.dim),
                Span::styled(label.clone(), (style.label_style)(theme)),
            ])]
        },
    }
}

// ─── Body rendering ───────────────────────────────────────────────────────

/// Render the body of a message. Checks for `RenderSpec` first;
/// falls back to plain text (with markdown for non-streaming Assistant).
fn render_body(
    msg: &Message,
    prefix: &str,
    body_style: Style,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if let Some(spec) = msg.body.render_spec() {
        let mut lines = Vec::new();
        render_spec_inner(spec, &mut lines, width, theme, prefix);
        return lines;
    }

    let text = msg.body.plain_text();
    if text.trim().is_empty() {
        return Vec::new();
    }

    let content_width = width.saturating_sub(text_width(prefix)).max(1);
    if msg.role == MessageRole::Assistant && !msg.is_streaming {
        let styles = MarkdownStyles::assistant(theme, body_style);
        let mut lines = Vec::new();
        render_markdown_to_lines(text, &mut lines, content_width, prefix, styles);
        lines
    } else {
        visual_lines(text, content_width)
            .into_iter()
            .map(|line| {
                Line::from(vec![
                    Span::styled(prefix.to_string(), body_style),
                    Span::styled(line, body_style),
                ])
            })
            .collect()
    }
}

fn text_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
        .sum()
}

// ─── Main dispatchers ─────────────────────────────────────────────────────

pub fn scrollback_entry_to_lines(
    entry: &ScrollbackEntry,
    width: u16,
    theme: &Theme,
    message_renderers: &MessageRendererRegistry,
) -> Vec<Line<'static>> {
    match entry {
        ScrollbackEntry::Message(msg) => render_message(msg, width, theme, message_renderers),
        ScrollbackEntry::StreamHeader => vec![Line::from("")],
        ScrollbackEntry::StreamText { role, text } => {
            stream_text_to_lines(role, text, width, theme)
        },
        ScrollbackEntry::BlankLine => vec![Line::from("")],
    }
}

/// Render a single `Message` into terminal lines.
///
/// Dispatches through `MessageRendererRegistry` for extension messages,
/// then applies role-specific header/body/separator rendering.
pub fn render_message(
    msg: &Message,
    width: u16,
    theme: &Theme,
    message_renderers: &MessageRendererRegistry,
) -> Vec<Line<'static>> {
    let content_width = width.max(1) as usize;

    // Extension messages: hand off to registered custom renderers.
    if let Some(custom_type) = msg.body.custom_type.as_deref() {
        if let Some(renderer) = message_renderers.get(custom_type) {
            let payload = msg
                .body
                .payload
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let opts = crate::tui::ext::message::MessageRenderOpts;
            if let Some(spec) = renderer.render(&payload, &opts) {
                let mut lines = Vec::new();
                render_spec_inner(&spec, &mut lines, content_width, theme, "  ");
                return lines;
            }
        }
    }

    let style = role_style(&msg.role);
    let body_style = (style.body_style)(theme);
    let mut lines = Vec::new();

    // Compact codex one-liner for plain Tool messages (no RenderSpec).
    if msg.role == MessageRole::Tool && msg.body.render_spec().is_none() {
        let text = msg.body.plain_text();
        let first_line = text.lines().next().unwrap_or("").trim();
        lines.push(Line::from(vec![
            Span::styled("  ⎿ ", theme.dim),
            Span::styled(msg.label.clone(), theme.tool_label),
            Span::styled("  ", theme.dim),
            Span::styled(first_line.to_string(), theme.dim),
        ]));
        return lines;
    }

    // Header.
    lines.extend(render_header(&style, msg, theme));

    // Body.
    lines.extend(render_body(
        msg,
        style.body_prefix,
        body_style,
        content_width,
        theme,
    ));

    // Streaming indicator.
    if msg.is_streaming {
        lines.push(Line::from(Span::styled("  ⋯", theme.dim)));
    }

    // Separator blank line.
    if style.separator {
        lines.push(Line::from(""));
    }

    lines
}

// ─── Streaming text ───────────────────────────────────────────────────────

fn stream_text_to_lines(
    role: &MessageRole,
    text: &str,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_width = (width as usize).saturating_sub(2).max(1);
    let style = if *role == MessageRole::Error {
        theme.error_label
    } else {
        theme.body
    };
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
