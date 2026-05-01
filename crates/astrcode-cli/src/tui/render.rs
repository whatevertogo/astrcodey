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
    state::{Focus, Message, MessageRole, ScrollbackEntry, TuiState},
    theme::Theme,
};

/// 主渲染入口：只渲染底部面板。
pub fn render(state: &TuiState, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    let footer_height = area.height.min(1);
    let status_height = if state.error.is_some() && area.height > footer_height {
        1
    } else {
        0
    };
    let can_show_live = status_height == 0
        && has_live_activity(state)
        && area.height > footer_height + status_height + 2;
    let live_height = u16::from(can_show_live);
    let composer_available = area
        .height
        .saturating_sub(footer_height + status_height + live_height);
    let composer_height = composer_available;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(status_height),
            Constraint::Length(live_height),
            Constraint::Length(composer_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    if status_height > 0 {
        render_status(state, frame, layout[0], theme);
    }
    if live_height > 0 {
        render_live_activity(state, frame, layout[1], theme);
    }
    render_composer(state, frame, layout[2], theme);
    render_footer(state, frame, layout[3], theme);

    if state.show_slash_palette {
        render_slash_palette(state, frame, area, theme);
    }
}

/// 将单条消息渲染为行列表，供 `insert_before()` 写入 scrollback。
pub fn message_to_lines(msg: &Message, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let content_width = width.max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let body_style = body_style(&msg.role, theme);

    // 角色前缀行
    let (role_icon, role_style) = role_icon_and_style(&msg.role, theme);
    lines.push(Line::from(vec![Span::styled(
        format!("{} {}", role_icon, msg.label),
        role_style,
    )]));

    // 有 RenderSpec 时使用结构化渲染，否则 fallback 到 plain_text。
    // 完成态 assistant 普通文本在前端本地按 block-first Markdown 解释，
    // streaming 分片保持原样写入 scrollback，避免最终整段重排。
    if let Some(spec) = msg.body.render_spec() {
        render_spec_to_lines(spec, &mut lines, content_width, theme, "  ");
    } else {
        let text = msg.body.plain_text();
        if !text.trim().is_empty() {
            if msg.role == MessageRole::Assistant && !msg.is_streaming {
                let styles = MarkdownStyles::assistant(theme, body_style);
                render_markdown_to_lines(text, &mut lines, content_width, "  ", styles);
            } else {
                let wrapped = visual_lines(text, content_width);
                for line in wrapped {
                    lines.push(Line::from(vec![
                        Span::styled("  ", theme.dim),
                        Span::styled(line, body_style),
                    ]));
                }
            }
        }
    }

    if msg.is_streaming {
        lines.push(Line::from(vec![
            Span::styled("  ⎿ ", theme.dim),
            Span::styled("running...", theme.dim),
        ]));
    }

    lines.push(Line::from(""));
    lines
}

/// 将 scrollback 条目渲染为行列表，供 `insert_before()` 写入。
pub fn scrollback_entry_to_lines(
    entry: &ScrollbackEntry,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match entry {
        ScrollbackEntry::Message(message) => message_to_lines(message, width, theme),
        ScrollbackEntry::StreamHeader { role, label } => {
            let (role_icon, role_style) = role_icon_and_style(role, theme);
            vec![Line::from(vec![Span::styled(
                format!("{} {}", role_icon, label),
                role_style,
            )])]
        },
        ScrollbackEntry::StreamText { role, text } => {
            let content_width = width.saturating_sub(2).max(1) as usize;
            let style = body_style(role, theme);
            visual_lines(text, content_width)
                .into_iter()
                .map(|line| {
                    Line::from(vec![
                        Span::styled("  ", theme.dim),
                        Span::styled(line, style),
                    ])
                })
                .collect()
        },
        ScrollbackEntry::BlankLine => vec![Line::from("")],
    }
}

fn role_icon_and_style(role: &MessageRole, theme: &Theme) -> (&'static str, Style) {
    match role {
        MessageRole::User => ("›", theme.user_label),
        MessageRole::Assistant => ("●", theme.assistant_label),
        MessageRole::Tool => ("⏺", theme.tool_label),
        MessageRole::System => ("•", theme.system_label),
        MessageRole::Error => ("✖", theme.error_label),
    }
}

fn body_style(role: &MessageRole, theme: &Theme) -> Style {
    if *role == MessageRole::Error {
        theme.body.patch(theme.error_label)
    } else {
        theme.body
    }
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

#[derive(Debug, Clone, Copy)]
struct MarkdownStyles {
    body: Style,
    heading: Style,
    marker: Style,
    code: Style,
}

impl MarkdownStyles {
    fn assistant(theme: &Theme, body: Style) -> Self {
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

/// 渲染 v1 block-first Markdown 子集。
///
/// 这里故意只支持适合原生 scrollback 的块级结构：ATX 标题、列表、
/// 引用、fenced code、段落和水平分隔线。inline emphasis/code 等复杂语法
/// 保持普通文本，避免为了终端 transcript 引入完整 Markdown 解析器。
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
                &format!("• {}", item),
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
                &format!("│ {quote}"),
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
        .find_map(|marker| line.strip_prefix(marker).map(str::trim_start))
}

fn parse_ordered_list(line: &str) -> Option<(&str, &str)> {
    let digit_end = line
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())
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
    let separator_width = width.saturating_sub(prefix_width).clamp(3, 40);
    push_wrapped_line_with_prefix_style(
        lines,
        prefix,
        style,
        &"─".repeat(separator_width),
        style,
        width,
    );
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

// ─── Status / Composer / Footer / Slash palette (unchanged) ─────────────────

fn render_status(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
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

fn render_live_activity(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Text::from(activity_lines(state, area.width, theme))),
        area,
    );
}

fn render_composer(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let active = state.focus == Focus::Input || state.focus == Focus::SlashPalette;
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(if active {
            theme.border_active
        } else {
            theme.border
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_width = inner.width.max(1);
    let (lines, cursor) = composer_lines_and_cursor(state, content_width);
    let styled_lines: Vec<Line> = if state.input_text().is_empty() {
        vec![Line::from(vec![
            Span::styled("› ", theme.assistant_label),
            Span::styled(
                "Ask astrcode to inspect, edit, or explain...",
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
    if area.height == 0 {
        return;
    }
    let session = state
        .active_session_id
        .as_deref()
        .map(super::short_id)
        .unwrap_or("none");
    let model = if state.model_name.is_empty() {
        "model: pending".to_string()
    } else {
        state.model_name.clone()
    };
    let cwd = if state.working_dir.is_empty() {
        "cwd pending".into()
    } else {
        compact_path(&state.working_dir)
    };
    let hints = if state.is_streaming {
        "Esc stop"
    } else {
        "Enter send · Shift+Enter newline · /help"
    };
    let line = fit_line(
        &format!("  {model} · {cwd} · session {session}   {hints}"),
        area.width as usize,
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
    let max_height = area.height.saturating_sub(1).max(1);
    let visible_items = commands
        .len()
        .min(max_height.saturating_sub(2).max(1) as usize);
    let selected = state.slash_selected.min(commands.len().saturating_sub(1));
    let start = selected.saturating_add(1).saturating_sub(visible_items);
    let height = (visible_items as u16 + 2).min(max_height);
    let popup = bottom_popup_rect(area, 70, height);
    let inner = popup.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let lines: Vec<Line> = commands
        .iter()
        .skip(start)
        .take(visible_items)
        .enumerate()
        .map(|(idx, command)| {
            let command_index = start + idx;
            let is_selected = command_index == selected;
            let label_style = if is_selected {
                theme.popup_selected
            } else {
                theme.assistant_label
            };
            let desc_style = if is_selected { theme.body } else { theme.dim };
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

fn bottom_popup_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = ((area.width as u32 * percent_x as u32) / 100) as u16;
    let popup_width = width.max(24).min(area.width);
    let popup_height = height.min(area.height);
    let bottom_gap = 3u16.min(area.height.saturating_sub(popup_height));
    Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + area.height.saturating_sub(popup_height + bottom_gap),
        width: popup_width,
        height: popup_height,
    }
}

// ─── 输入编辑器布局 ───────────────────────────────────────────────────

fn composer_lines_and_cursor(state: &TuiState, width: u16) -> (Vec<String>, (u16, u16)) {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let layout = layout_visual_text(
        state.input_text(),
        content_width,
        Some(state.input_cursor()),
    );
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

fn has_live_activity(state: &TuiState) -> bool {
    latest_live_message(state).is_some() || should_show_status_activity(state)
}

fn activity_lines(state: &TuiState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if let Some(message) = latest_live_message(state) {
        let (icon, style) = match message.role {
            MessageRole::Assistant => ("●", theme.assistant_label),
            MessageRole::Tool => ("⏺", theme.tool_label),
            MessageRole::Error => ("✖", theme.error_label),
            MessageRole::User => ("›", theme.user_label),
            MessageRole::System => ("•", theme.system_label),
        };
        let prefix = format!("  {icon} ");
        let mut text = message.label.clone();
        if message.role == MessageRole::Assistant {
            text.push_str(" streaming");
        } else if let Some(summary) = compact_activity_summary(message.body.plain_text()) {
            text.push_str(" · ");
            text.push_str(&summary);
        }
        let available = width.saturating_sub(text_width(&prefix) as u16).max(1) as usize;
        let text = fit_line(&text, available);
        return vec![Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(text, style),
        ])];
    }

    if should_show_status_activity(state) {
        let prefix = "  ● ";
        let available = width.saturating_sub(text_width(prefix) as u16).max(1) as usize;
        let status = fit_line(&state.status, available);
        return vec![Line::from(vec![
            Span::styled(prefix, theme.status_busy),
            Span::styled(status, theme.body),
        ])];
    }

    Vec::new()
}

fn latest_live_message(state: &TuiState) -> Option<&Message> {
    state
        .messages
        .iter()
        .rev()
        .find(|message| message.is_streaming)
}

fn should_show_status_activity(state: &TuiState) -> bool {
    state.is_streaming
        || (!state.status.is_empty()
            && !state.status.starts_with("Ready")
            && !state.status.ends_with("session(s)")
            && state.status != "Ready")
}

fn compact_activity_summary(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn compact_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<_> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() <= 3 {
        return normalized;
    }
    let root = if normalized.contains(":/") {
        parts.first().copied().unwrap_or_default()
    } else if normalized.starts_with('/') {
        ""
    } else {
        parts.first().copied().unwrap_or_default()
    };
    let tail = &parts[parts.len().saturating_sub(2)..];
    if root.is_empty() {
        format!("/.../{}", tail.join("/"))
    } else {
        format!("{root}/.../{}", tail.join("/"))
    }
}

fn fit_line(text: &str, width: usize) -> String {
    if width == 0 || text_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".into();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width + 1 > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
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
        let mut lines = Vec::new();

        render_spec_to_lines(&spec, &mut lines, 48, &theme, "  ");

        let texts = line_texts(&lines);
        assert!(texts.iter().any(|line| line == "  Title"));
        assert!(texts.iter().any(|line| line == "  • first"));
        assert!(texts.iter().any(|line| line == "  2. second"));
        assert!(texts.iter().any(|line| line == "  │ quoted"));
        assert!(texts.iter().any(|line| line.starts_with("  ───")));
        assert!(texts.iter().any(|line| line == "  code rust"));
        assert!(texts.iter().any(|line| line == "      let x = 1;"));
        assert!(!texts.iter().any(|line| line.contains("# Title")));
        assert!(!texts.iter().any(|line| line.contains("```")));
    }

    #[test]
    fn assistant_plain_text_markdown_renders_at_frontend_boundary() {
        let theme = Theme::detect();
        let mut state = TuiState::new();
        state.push_message(
            MessageRole::Assistant,
            "Astrcode".into(),
            "# Done\n- item".into(),
            false,
            None,
        );

        let lines = message_to_lines(&state.messages[0], 48, &theme);
        let texts = line_texts(&lines);

        assert!(texts.iter().any(|line| line == "  Done"));
        assert!(texts.iter().any(|line| line == "  • item"));
        assert!(!texts.iter().any(|line| line.contains("# Done")));
    }

    #[test]
    fn markdown_tone_is_preserved_for_error_output() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "# Failure\n- bad".into(),
            tone: RenderTone::Error,
        };
        let mut lines = Vec::new();

        render_spec_to_lines(&spec, &mut lines, 48, &theme, "  ");

        let failure = lines
            .iter()
            .find(|line| line_text(line) == "  Failure")
            .expect("heading should render without markdown marker");
        assert_eq!(failure.spans[1].style, theme.error_label);
    }

    #[test]
    fn markdown_respects_parent_prefix_when_wrapping() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "- alpha beta gamma delta".into(),
            tone: RenderTone::Default,
        };
        let mut lines = Vec::new();

        render_spec_to_lines(&spec, &mut lines, 18, &theme, "  ⎿ ");

        let texts = line_texts(&lines);
        assert!(texts[0].starts_with("  ⎿ •"));
        assert!(texts[1].starts_with("    "));
    }

    #[test]
    fn unsupported_inline_markdown_stays_plain_text() {
        let theme = Theme::detect();
        let spec = RenderSpec::Markdown {
            text: "Keep **bold** and `code` literal".into(),
            tone: RenderTone::Default,
        };
        let mut lines = Vec::new();

        render_spec_to_lines(&spec, &mut lines, 80, &theme, "  ");

        let texts = line_texts(&lines);
        assert!(
            texts
                .iter()
                .any(|line| line == "  Keep **bold** and `code` literal")
        );
    }
}
