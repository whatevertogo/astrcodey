//! Claude Code 风格渲染层：上方为紧凑消息记录，下方为聚焦的输入编辑器。
//!
//! 负责将 TUI 状态转换为 ratatui 组件并绘制到终端帧上。
//! 包含消息记录、状态栏、输入编辑器、底部信息栏和斜杠命令面板的渲染逻辑，
//! 以及基于 Unicode 显示宽度的文本换行计算。

use astrcode_core::render::{RenderSpec, RenderTone};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use super::{
    slash,
    state::{Focus, Message, MessageRole, TuiState},
    theme::Theme,
};

const MAX_RAW_ANSI_CHARS: usize = 4000;

/// 主渲染入口：将 TUI 状态绘制到终端帧上。
///
/// 布局从上到下依次为：消息记录区、状态栏（可选）、输入编辑器、底部信息栏。
/// 斜杠命令面板作为浮动弹窗叠加显示。
pub fn render(state: &TuiState, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    // 根据输入内容动态计算编辑器高度，至少 3 行，最多不超过终端高度减 2
    let composer_height = composer_height(state, area.width)
        .min(area.height.saturating_sub(2))
        .max(3);
    // 仅在流式输出中或存在错误时显示状态栏
    let status_height = if state.is_streaming || state.error.is_some() {
        1
    } else {
        0
    };
    // 垂直布局：消息区（弹性）→ 状态栏 → 编辑器 → 底部栏
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_height),
            Constraint::Length(composer_height),
            Constraint::Length(2),
        ])
        .split(area);

    render_transcript(state, frame, layout[0], theme);
    if status_height > 0 {
        render_status(state, frame, layout[1], theme);
    }
    render_composer(state, frame, layout[2], theme);
    render_footer(state, frame, layout[3], theme);

    // 斜杠命令面板作为浮动弹窗渲染
    if state.show_slash_palette {
        render_slash_palette(state, frame, area, theme);
    }
}

/// 渲染消息记录区域：显示用户、助手、工具等消息的滚动视图。
fn render_transcript(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let lines = build_transcript_lines(state, area.width, theme);
    // 裁剪到可视区域；默认显示最新消息，用户滚动后保留距离底部的偏移
    let visible = clip_to_window(lines, area.height as usize, state.transcript_scroll);
    let paragraph = Paragraph::new(Text::from(visible));
    frame.render_widget(paragraph, area);
}

/// 渲染状态栏：显示错误信息或当前工作状态。
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

/// 渲染输入编辑器：Claude Code 风格上下线输入区域，支持光标定位和占位提示。
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
    // 输入为空时显示占位提示文本
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
                // 首行显示 › 前缀，续行缩进对齐
                let prefix = if idx == 0 { "› " } else { "  " };
                Line::from(vec![
                    Span::styled(prefix, theme.assistant_label),
                    Span::styled(line, theme.composer),
                ])
            })
            .collect()
    };

    frame.render_widget(Paragraph::new(Text::from(styled_lines)), inner);

    // 激活状态下设置光标位置，限制在可视区域内
    if active {
        let cursor_x = inner.x + cursor.0.min(inner.width.saturating_sub(1));
        let cursor_y = inner.y + cursor.1.min(inner.height.saturating_sub(1));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// 渲染底部信息栏：显示模型名称、会话 ID 和快捷键提示。
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
        "  › Enter send · Shift+Enter newline · wheel/pg scroll · Esc stop · /quit exit".into()
    } else {
        format!(
            "  › Enter send · Shift+Enter newline · sessions {} · wheel/pg scroll · /quit exit",
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

/// 渲染斜杠命令面板：居中弹窗，显示匹配的命令列表。
fn render_slash_palette(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let commands = slash::filtered(&state.slash_filter);
    if commands.is_empty() {
        return;
    }

    // 最多显示 6 条命令，加上边框共需高度
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

    // 先清除弹窗区域，再绘制边框和内容
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

/// 构建消息记录的所有渲染行。
///
/// 最多显示最近 120 条消息，每条消息包含角色标签和正文内容。
/// 正文按可视宽度自动换行，流式消息末尾显示 "streaming…" 指示。
fn build_transcript_lines(state: &TuiState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let content_width = width.max(1) as usize;
    let mut lines = Vec::new();

    // 无消息时显示欢迎提示
    if state.messages.is_empty() {
        return vec![
            Line::from(vec![
                Span::styled("✻ ", theme.assistant_label),
                Span::styled("Astrcode", theme.assistant_label),
            ]),
            Line::from(Span::styled(
                "  Welcome back. Type below to inspect, edit, or explain.",
                theme.dim,
            )),
        ];
    }

    // 取最近 120 条消息（保留时间顺序）
    for message in state.messages.iter().rev().take(120).rev() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }

        // 错误消息使用错误样式
        let body_style = if message.role == MessageRole::Error {
            theme.body.patch(theme.error_label)
        } else {
            theme.body
        };

        match message.role {
            MessageRole::User => push_message_body_lines(
                &mut lines,
                message,
                BodyLineRender {
                    first_prefix: "› ",
                    next_prefix: "  ",
                    prefix_style: theme.user_label,
                    body_style,
                    content_width,
                    theme,
                },
            ),
            MessageRole::Assistant => push_message_body_lines(
                &mut lines,
                message,
                BodyLineRender {
                    first_prefix: "● ",
                    next_prefix: "  ",
                    prefix_style: theme.assistant_label,
                    body_style,
                    content_width,
                    theme,
                },
            ),
            MessageRole::Tool => {
                lines.push(Line::from(vec![
                    Span::styled("⏺ ", theme.tool_label),
                    Span::styled(message.label.clone(), theme.tool_label),
                ]));
                push_message_body_lines(
                    &mut lines,
                    message,
                    BodyLineRender {
                        first_prefix: "  ⎿ ",
                        next_prefix: "    ",
                        prefix_style: theme.dim,
                        body_style,
                        content_width,
                        theme,
                    },
                );
            },
            MessageRole::System => {
                lines.push(Line::from(vec![
                    Span::styled("• ", theme.system_label),
                    Span::styled(message.label.clone(), theme.system_label),
                ]));
                push_message_body_lines(
                    &mut lines,
                    message,
                    BodyLineRender {
                        first_prefix: "  ",
                        next_prefix: "  ",
                        prefix_style: theme.dim,
                        body_style,
                        content_width,
                        theme,
                    },
                );
            },
            MessageRole::Error => {
                lines.push(Line::from(vec![
                    Span::styled("✖ ", theme.error_label),
                    Span::styled(message.label.clone(), theme.error_label),
                ]));
                push_message_body_lines(
                    &mut lines,
                    message,
                    BodyLineRender {
                        first_prefix: "  ⎿ ",
                        next_prefix: "    ",
                        prefix_style: theme.dim,
                        body_style,
                        content_width,
                        theme,
                    },
                );
            },
        }

        // 流式输出中的消息末尾显示指示
        if message.is_streaming {
            lines.push(Line::from(vec![
                Span::styled("  ⎿ ", theme.dim),
                Span::styled("streaming…", theme.dim),
            ]));
        }
    }

    lines
}

#[derive(Clone, Copy)]
struct BodyLineRender<'a> {
    first_prefix: &'static str,
    next_prefix: &'static str,
    prefix_style: Style,
    body_style: Style,
    content_width: usize,
    theme: &'a Theme,
}

fn push_message_body_lines(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    render: BodyLineRender<'_>,
) {
    if let Some(spec) = message.body.render_spec() {
        let rendered = render_spec_to_lines_with_prefix(
            spec,
            render.content_width,
            render.theme,
            render.first_prefix,
            render.prefix_style,
        );
        if rendered.is_empty() {
            push_prefixed_text_lines(lines, message.body.plain_text(), render);
        } else {
            lines.extend(rendered);
        }
    } else {
        push_prefixed_text_lines(lines, message.body.plain_text(), render);
    }
}

fn push_prefixed_text_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    render: BodyLineRender<'_>,
) {
    let prefix_width =
        string_display_width(render.first_prefix).max(string_display_width(render.next_prefix));
    let wrapped = visual_lines(
        text,
        render.content_width.saturating_sub(prefix_width).max(1),
    );
    if wrapped.is_empty() {
        // 空内容显示省略号
        lines.push(Line::from(vec![
            Span::styled(render.first_prefix.to_string(), render.prefix_style),
            Span::styled("…", render.theme.dim),
        ]));
    } else {
        for (idx, line) in wrapped.into_iter().enumerate() {
            let prefix = if idx == 0 {
                render.first_prefix
            } else {
                render.next_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), render.prefix_style),
                Span::styled(line, render.body_style),
            ]));
        }
    }
}

fn render_spec_to_lines_with_prefix(
    spec: &RenderSpec,
    width: usize,
    theme: &Theme,
    prefix: &str,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_spec_node(spec, width.max(1), theme, prefix, prefix_style, &mut lines);
    lines
}

fn render_spec_node(
    spec: &RenderSpec,
    width: usize,
    theme: &Theme,
    prefix: &str,
    prefix_style: Style,
    lines: &mut Vec<Line<'static>>,
) {
    match spec {
        RenderSpec::Text { text, tone } | RenderSpec::Markdown { text, tone } => {
            push_wrapped_render_lines(
                lines,
                prefix,
                prefix_style,
                text,
                tone_style(tone, theme),
                width,
                theme,
            );
        },
        RenderSpec::Box {
            title,
            tone,
            children,
        } => {
            if let Some(title) = title {
                push_wrapped_render_lines(
                    lines,
                    prefix,
                    prefix_style,
                    &format!("• {title}"),
                    tone_style(tone, theme),
                    width,
                    theme,
                );
            }
            let child_prefix = format!("{prefix}  ⎿ ");
            for child in children {
                render_spec_node(child, width, theme, &child_prefix, prefix_style, lines);
            }
            if title.is_none() && children.is_empty() {
                push_wrapped_render_lines(
                    lines,
                    prefix,
                    prefix_style,
                    "…",
                    theme.dim,
                    width,
                    theme,
                );
            }
        },
        RenderSpec::List {
            ordered,
            items,
            tone,
        } => {
            for (idx, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", idx + 1)
                } else {
                    "•".into()
                };
                let item_prefix = format!("{prefix}{marker} ");
                render_spec_node_with_default_tone(
                    item,
                    width,
                    theme,
                    &item_prefix,
                    prefix_style,
                    lines,
                    tone,
                );
            }
        },
        RenderSpec::KeyValue { entries, tone } => {
            for entry in entries {
                let style = if entry.tone == RenderTone::Default {
                    tone_style(tone, theme)
                } else {
                    tone_style(&entry.tone, theme)
                };
                push_wrapped_render_lines(
                    lines,
                    prefix,
                    prefix_style,
                    &format!("{}: {}", entry.key, entry.value),
                    style,
                    width,
                    theme,
                );
            }
        },
        RenderSpec::Progress {
            label,
            status,
            value,
            tone,
        } => {
            let mut text = format!("• {label}");
            if let Some(status) = status {
                text.push_str(" · ");
                text.push_str(status);
            }
            if let Some(value) = value {
                text.push_str(&format!(" · {:.0}%", value.clamp(0.0, 1.0) * 100.0));
            }
            push_wrapped_render_lines(
                lines,
                prefix,
                prefix_style,
                &text,
                tone_style(tone, theme),
                width,
                theme,
            );
        },
        RenderSpec::Diff { text, tone } => {
            for line in text.lines() {
                let style = match line.chars().next() {
                    Some('+') => tone_style(&RenderTone::Success, theme),
                    Some('-') => tone_style(&RenderTone::Error, theme),
                    _ => tone_style(tone, theme),
                };
                push_wrapped_render_lines(lines, prefix, prefix_style, line, style, width, theme);
            }
        },
        RenderSpec::Code {
            language,
            text,
            tone,
        } => {
            if let Some(language) = language {
                push_wrapped_render_lines(
                    lines,
                    prefix,
                    prefix_style,
                    &format!("```{language}"),
                    theme.dim,
                    width,
                    theme,
                );
            }
            for line in text.lines() {
                push_wrapped_render_lines(
                    lines,
                    prefix,
                    prefix_style,
                    line,
                    tone_style(tone, theme),
                    width,
                    theme,
                );
            }
        },
        RenderSpec::ImageRef { uri, alt, tone } => {
            push_wrapped_render_lines(
                lines,
                prefix,
                prefix_style,
                &format!("[image: {}]", alt.as_deref().unwrap_or(uri)),
                tone_style(tone, theme),
                width,
                theme,
            );
        },
        RenderSpec::RawAnsiLimited { text, tone } => {
            let safe = strip_ansi_limited(text);
            push_wrapped_render_lines(
                lines,
                prefix,
                prefix_style,
                &safe,
                tone_style(tone, theme),
                width,
                theme,
            );
        },
    }
}

fn render_spec_node_with_default_tone(
    spec: &RenderSpec,
    width: usize,
    theme: &Theme,
    prefix: &str,
    prefix_style: Style,
    lines: &mut Vec<Line<'static>>,
    fallback_tone: &RenderTone,
) {
    if matches!(
        spec,
        RenderSpec::Text {
            tone: RenderTone::Default,
            ..
        }
    ) {
        let RenderSpec::Text { text, .. } = spec else {
            return;
        };
        push_wrapped_render_lines(
            lines,
            prefix,
            prefix_style,
            text,
            tone_style(fallback_tone, theme),
            width,
            theme,
        );
        return;
    }
    render_spec_node(spec, width, theme, prefix, prefix_style, lines);
}

fn push_wrapped_render_lines(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    text: &str,
    style: Style,
    width: usize,
    theme: &Theme,
) {
    let prefix_width = string_display_width(prefix);
    let wrapped = visual_lines(text, width.saturating_sub(prefix_width).max(1));
    if wrapped.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::styled("…", theme.dim),
        ]));
        return;
    }
    for line in wrapped {
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::styled(line, style),
        ]));
    }
}

fn tone_style(tone: &RenderTone, theme: &Theme) -> Style {
    match tone {
        RenderTone::Default => theme.body,
        RenderTone::Muted => theme.dim,
        RenderTone::Accent => theme.assistant_label,
        RenderTone::Success => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        RenderTone::Warning => theme.tool_label,
        RenderTone::Error => theme.error_label,
    }
}

fn strip_ansi_limited(text: &str) -> String {
    let mut output = String::new();
    let mut visible_chars = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if visible_chars >= MAX_RAW_ANSI_CHARS {
            output.push('…');
            break;
        }
        if ch == '\u{1b}' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch == '\u{9b}' {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            output.push(ch);
            visible_chars += 1;
        }
    }
    output
}

fn string_display_width(text: &str) -> usize {
    text.chars().map(display_width).sum()
}

/// 计算输入编辑器的文本行和光标位置。
///
/// 返回 (文本行列表, (光标列, 光标行))，列偏移包含 "› " 前缀的 2 个字符。
fn composer_lines_and_cursor(state: &TuiState, width: u16) -> (Vec<String>, (u16, u16)) {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let layout = layout_visual_text(&state.input, content_width, Some(state.input_cursor));
    let lines = layout.lines;
    // 光标位置加上 "› " 前缀的 2 列偏移
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

/// 将文本按可视宽度换行，返回行列表（不含光标信息）。
fn visual_lines(text: &str, width: usize) -> Vec<String> {
    layout_visual_text(text, width, None).lines
}

/// 按距离底部的偏移裁剪行列表。
fn clip_to_window(
    lines: Vec<Line<'static>>,
    height: usize,
    scroll_from_bottom: usize,
) -> Vec<Line<'static>> {
    if height == 0 || lines.len() <= height {
        return lines;
    }
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = scroll_from_bottom.min(max_scroll);
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    lines[start..end].to_vec()
}

/// 计算居中弹窗的矩形区域。
///
/// `percent_x` 为弹窗宽度占终端宽度的百分比，`height` 为弹窗高度。
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

/// 根据输入内容计算编辑器所需高度（行数 + 边框），最大 8 行。
fn composer_height(state: &TuiState, width: u16) -> u16 {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let lines = visual_lines(&state.input, content_width).len().max(1) as u16;
    (lines + 2).min(8)
}


/// 文本可视布局结果：换行后的文本行及可选的光标位置。
#[derive(Debug, Default)]
struct VisualLayout {
    /// 换行后的文本行
    lines: Vec<String>,
    /// 光标所在行索引
    cursor_row: Option<usize>,
    /// 光标所在列（以显示宽度计算）
    cursor_column: Option<usize>,
}

/// 将文本按可视宽度进行换行布局，同时追踪光标位置。
///
/// 正确处理 Unicode 字符的显示宽度（如 CJK 字符占 2 列）和换行符。
/// `cursor` 参数为光标在原始文本中的字符偏移量（按 char 索引）。
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

    // 处理光标在文本最开头的情况
    if cursor == Some(0) {
        layout.cursor_row = Some(0);
        layout.cursor_column = Some(0);
    }

    for ch in text.chars() {
        // 在处理字符前记录光标位置
        if cursor == Some(consumed_chars) {
            layout.cursor_row = Some(current_row);
            layout.cursor_column = Some(current_width);
        }

        if ch == '\n' {
            // 硬换行：结束当前行
            layout.lines.push(std::mem::take(&mut current_line));
            current_width = 0;
            current_row += 1;
            consumed_chars += 1;

            // 换行后光标位于新行首
            if cursor == Some(consumed_chars) {
                layout.cursor_row = Some(current_row);
                layout.cursor_column = Some(0);
            }
            continue;
        }

        let ch_width = display_width(ch);
        // 软换行：当前行放不下时结束当前行
        if current_width + ch_width > width && !current_line.is_empty() {
            layout.lines.push(std::mem::take(&mut current_line));
            current_width = 0;
            current_row += 1;

            // 软换行时光标位于新行首
            if cursor == Some(consumed_chars) {
                layout.cursor_row = Some(current_row);
                layout.cursor_column = Some(0);
            }
        }

        current_line.push(ch);
        current_width += ch_width;
        consumed_chars += 1;

        // 字符追加后更新光标位置
        if cursor == Some(consumed_chars) {
            layout.cursor_row = Some(current_row);
            layout.cursor_column = Some(current_width);
        }
    }

    // 处理光标在文本末尾的情况
    if cursor == Some(consumed_chars) {
        layout.cursor_row = Some(current_row);
        layout.cursor_column = Some(current_width);
    }

    // 推入最后一行（可能为空字符串）
    layout.lines.push(current_line);
    layout
}

/// 计算单个字符的终端显示宽度。
///
/// CJK 字符宽度为 2，控制字符宽度为 0（最小返回 1 以避免布局异常）。
fn display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).max(1)
}

#[cfg(test)]
mod tests {
    use astrcode_core::render::{RenderKeyValue, RenderTone};

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

    #[test]
    fn transcript_clip_scrolls_from_bottom() {
        let lines = ["one", "two", "three", "four"]
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();

        let visible = clip_to_window(lines, 2, 1)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert_eq!(visible, vec!["two", "three"]);
    }

    #[test]
    fn render_spec_box_uses_claude_tree_lines() {
        let theme = Theme::detect();
        let spec = RenderSpec::Box {
            title: Some("Search".into()),
            tone: RenderTone::Accent,
            children: vec![RenderSpec::KeyValue {
                entries: vec![RenderKeyValue {
                    key: "matches".into(),
                    value: "3".into(),
                    tone: RenderTone::Success,
                }],
                tone: RenderTone::Default,
            }],
        };

        let lines = render_spec_to_lines_with_prefix(&spec, 80, &theme, "  ", theme.dim)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert!(lines.iter().any(|line| line.contains("• Search")));
        assert!(lines.iter().any(|line| line.contains("⎿ matches: 3")));
    }

    #[test]
    fn raw_ansi_limited_strips_escape_sequences() {
        assert_eq!(strip_ansi_limited("\u{1b}[31mred\u{1b}[0m"), "red");
    }
}
