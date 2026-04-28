//! Codex 风格渲染层：上方为消息记录视图，下方为聚焦的输入编辑器。
//!
//! 负责将 TUI 状态转换为 ratatui 组件并绘制到终端帧上。
//! 包含消息记录、状态栏、输入编辑器、底部信息栏和斜杠命令面板的渲染逻辑，
//! 以及基于 Unicode 显示宽度的文本换行计算。

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
    state::{Focus, MessageRole, TuiState},
    theme::Theme,
};

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
            Constraint::Length(1),
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
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border);
    let inner = block.inner(area);
    let lines = build_transcript_lines(state, inner.width, theme);
    // 裁剪到可视区域，始终显示最新的消息（底部对齐）
    let visible = clip_to_bottom(lines, inner.height as usize);
    let paragraph = Paragraph::new(Text::from(visible)).block(block);
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

/// 渲染输入编辑器：带边框的文本输入区域，支持光标定位和占位提示。
fn render_composer(state: &TuiState, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let active = state.focus == Focus::Input || state.focus == Focus::SlashPalette;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if active {
            theme.border_active
        } else {
            theme.border
        })
        .title(" Composer ");
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
    let sessions = if state.available_sessions.is_empty() {
        String::new()
    } else {
        format!("  ·  sessions: {}", state.available_sessions.len())
    };
    let line = format!(
        "{}  ·  session: {}{}  ·  Enter send  ·  Shift+Enter newline  ·  Esc stop  ·  /quit exit",
        model, session, sessions
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(line, theme.footer))),
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
    let content_width = width.saturating_sub(2).max(1) as usize;
    let mut lines = Vec::new();

    // 无消息时显示欢迎提示
    if state.messages.is_empty() {
        return vec![
            Line::from(Span::styled("Astrcode", theme.assistant_label)),
            Line::from(Span::styled(
                "  Start typing below. This view now stays fully inside the TUI instead of \
                 spilling into terminal scrollback.",
                theme.dim,
            )),
        ];
    }

    // 取最近 120 条消息（保留时间顺序）
    for message in state.messages.iter().rev().take(120).rev() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }

        let label_style = message_label_style(&message.role, theme);
        lines.push(Line::from(Span::styled(message.label.clone(), label_style)));

        // 错误消息使用错误样式
        let body_style = if message.role == MessageRole::Error {
            theme.body.patch(theme.error_label)
        } else {
            theme.body
        };

        let wrapped = visual_lines(&message.content, content_width);
        if wrapped.is_empty() {
            // 空内容显示省略号
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled("…", theme.dim),
            ]));
        } else {
            for line in wrapped {
                lines.push(Line::from(vec![
                    Span::styled("  ", theme.dim),
                    Span::styled(line, body_style),
                ]));
            }
        }

        // 流式输出中的消息末尾显示指示
        if message.is_streaming {
            lines.push(Line::from(vec![
                Span::styled("  ", theme.dim),
                Span::styled("streaming…", theme.dim),
            ]));
        }
    }

    lines
}

/// 根据消息角色返回对应的标签样式。
fn message_label_style(role: &MessageRole, theme: &Theme) -> Style {
    match role {
        MessageRole::User => theme.user_label,
        MessageRole::Assistant => theme.assistant_label,
        MessageRole::Tool => theme.tool_label,
        MessageRole::System => theme.system_label,
        MessageRole::Error => theme.error_label,
    }
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

/// 裁剪行列表到底部指定高度，实现底部对齐的滚动效果。
fn clip_to_bottom(lines: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    if height == 0 || lines.len() <= height {
        return lines;
    }
    lines[lines.len() - height..].to_vec()
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
    let content_width = width.saturating_sub(4).max(1) as usize;
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
}
