//! 底部面板渲染层 + scrollback 消息行生成。
//!
//! transcript 内容通过 `insert_before()` 写入终端原生 scrollback，
//! 此处只渲染固定高度的底部 UI。

use astrcode_core::render::{RenderSpec, RenderTone};
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
    state::{Focus, Message, MessageRole, TuiState},
    theme::Theme,
};

/// 主渲染入口：只渲染底部面板。
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
            Constraint::Length(status_height),
            Constraint::Length(composer_height),
            Constraint::Length(2),
        ])
        .split(area);

    if status_height > 0 {
        render_status(state, frame, layout[0], theme);
    }
    render_composer(state, frame, layout[1], theme);
    render_footer(state, frame, layout[2], theme);

    if state.show_slash_palette {
        render_slash_palette(state, frame, area, theme);
    }
}

/// 将单条消息渲染为行列表，供 `insert_before()` 写入 scrollback。
pub fn message_to_lines(msg: &Message, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let content_width = width.max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let body_style = if msg.role == MessageRole::Error {
        theme.body.patch(theme.error_label)
    } else {
        theme.body
    };

    // 角色前缀行
    let (role_icon, role_style) = match msg.role {
        MessageRole::User => ("›", theme.user_label),
        MessageRole::Assistant => ("●", theme.assistant_label),
        MessageRole::Tool => ("⏺", theme.tool_label),
        MessageRole::System => ("•", theme.system_label),
        MessageRole::Error => ("✖", theme.error_label),
    };
    lines.push(Line::from(vec![Span::styled(
        format!("{} {}", role_icon, msg.label),
        role_style,
    )]));

    // 有 RenderSpec 时使用结构化渲染，否则 fallback 到 plain_text
    if let Some(spec) = msg.body.render_spec() {
        render_spec_to_lines(spec, &mut lines, content_width, theme, "  ");
    } else {
        let text = msg.body.plain_text();
        let wrapped = visual_lines(text, content_width);
        for line in wrapped {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled(line, body_style),
            ]));
        }
    }

    if msg.is_streaming {
        lines.push(Line::from(vec![
            Span::styled("  ⎿ ", theme.dim),
            Span::styled("streaming…", theme.dim),
        ]));
    }

    lines
}

/// 将 RenderSpec 递归渲染为带缩进的行列表。
fn render_spec_to_lines(
    spec: &RenderSpec,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: &Theme,
    prefix: &str,
) {
    match spec {
        RenderSpec::Text { text, tone: _ } | RenderSpec::Markdown { text, tone: _ } => {
            push_wrapped_line(lines, prefix, text, theme.body, width);
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
                    &format!("• {title}"),
                    theme.assistant_label,
                    width,
                );
            }
            let child_prefix = format!("{prefix}  ⎿ ");
            for child in children {
                render_spec_to_lines(child, lines, width, theme, &child_prefix);
            }
        },
        RenderSpec::List { items, tone: _, .. } => {
            for item in items {
                match item {
                    RenderSpec::Text { text, tone: _ } => {
                        push_wrapped_line(lines, prefix, &format!("• {text}"), theme.body, width);
                    },
                    other => {
                        let item_prefix = format!("{prefix}• ");
                        render_spec_to_lines(other, lines, width, theme, &item_prefix);
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
            let mut text = format!("• {label}");
            if let Some(status) = status {
                text.push_str(" · ");
                text.push_str(status);
            }
            if let Some(value) = value {
                text.push_str(&format!(" · {:.0}%", value.clamp(0.0, 1.0) * 100.0));
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

fn push_wrapped_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    let prefix_width = text_width(prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let wrapped = visual_lines(text, content_width);
    if wrapped.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), style),
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
            Span::styled(p, style),
            Span::styled(line.clone(), style),
        ]));
    }
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
        if ch == '\u{1b}' && chars.next() == Some('[') {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if ch == '\u{9b}' {
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

// ─── Status / Composer / Footer / Slash palette (unchanged) ─────────────────

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
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(if active {
            theme.border_active
        } else {
            theme.border
        });
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
        .map(super::short_id)
        .unwrap_or("none");
    let model = if state.model_name.is_empty() {
        "model: pending".to_string()
    } else {
        format!("model: {}", state.model_name)
    };
    let cwd = if state.working_dir.is_empty() {
        "cwd: pending"
    } else {
        state.working_dir.as_str()
    };
    let line = format!("{model} · {cwd} · session {session}");
    let hints = if state.available_sessions.is_empty() {
        "  › Enter send · Shift+Enter newline · /quit exit".into()
    } else {
        format!(
            "  › Enter send · Shift+Enter newline · sessions {} · /quit exit",
            state.available_sessions.len()
        )
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(format!("  {line}"), theme.footer)),
            Line::from(Span::styled(hints, theme.footer)),
        ])),
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

// ─── 输入编辑器布局 ───────────────────────────────────────────────────

fn composer_lines_and_cursor(state: &TuiState, width: u16) -> (Vec<String>, (u16, u16)) {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let layout = layout_visual_text(&state.input, content_width, Some(state.input_cursor));
    let cursor = (
        2 + layout.cursor_column.unwrap_or(0) as u16,
        layout.cursor_row.unwrap_or(0) as u16,
    );
    let lines_empty = layout.lines.is_empty();
    (layout.lines, if lines_empty { (2, 0) } else { cursor })
}

pub fn visual_lines(text: &str, width: usize) -> Vec<String> {
    layout_visual_text(text, width, None).lines
}

fn composer_height(state: &TuiState, width: u16) -> u16 {
    let content_width = width.saturating_sub(2).max(1) as usize;
    (visual_lines(&state.input, content_width).len().max(1) as u16 + 2).min(8)
}

// ─── 文本布局引擎 ─────────────────────────────────────────────────────

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
