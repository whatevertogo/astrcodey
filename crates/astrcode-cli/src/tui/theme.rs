//! Codex 风格终端主题：简洁的消息记录、聚焦的输入编辑器、清晰的状态强调色。
//!
//! 根据终端背景明暗自动选择配色方案，定义所有 UI 元素的样式常量。

use ratatui::style::{Color, Modifier, Style};

/// 终端 UI 主题，包含所有组件的样式定义。
///
/// 通过 [`Theme::detect()`] 自动检测终端明暗并生成对应配色。
#[derive(Debug, Clone)]
pub struct Theme {
    /// 普通边框样式
    pub border: Style,
    /// 激活状态边框样式（如聚焦的输入框）
    pub border_active: Style,
    /// 用户消息标签样式
    pub user_label: Style,
    /// 助手消息标签样式
    pub assistant_label: Style,
    /// 工具消息标签样式
    pub tool_label: Style,
    /// 系统消息标签样式
    pub system_label: Style,
    /// 错误消息标签样式
    pub error_label: Style,
    /// 正文内容样式
    pub body: Style,
    /// 次要/暗淡文本样式
    pub dim: Style,
    /// 状态栏文本样式
    pub status: Style,
    /// 忙碌状态标签样式
    pub status_busy: Style,
    /// 底部信息栏样式
    pub footer: Style,
    /// 输入编辑器文本样式
    pub composer: Style,
    /// 输入编辑器占位提示样式
    pub composer_placeholder: Style,
    /// 弹窗边框样式
    pub popup_border: Style,
    /// 弹窗选中项样式
    pub popup_selected: Style,
}

impl Theme {
    /// 自动检测终端明暗并生成对应主题。
    ///
    /// 通过 `TERM_PROGRAM` 环境变量判断是否为浅色终端（目前仅识别 Apple_Terminal），
    /// 其他终端默认使用深色配色方案。
    pub fn detect() -> Self {
        let dark = is_terminal_dark();
        // 次要文本颜色：深色终端用浅灰，浅色终端用深灰
        let muted = if dark {
            Color::Rgb(120, 129, 148)
        } else {
            Color::Rgb(96, 103, 120)
        };
        // 边框颜色
        let border = if dark {
            Color::Rgb(54, 62, 79)
        } else {
            Color::Rgb(196, 204, 222)
        };
        // 强调色（助手标签、激活边框等）
        let accent = if dark {
            Color::Rgb(112, 197, 255)
        } else {
            Color::Rgb(0, 120, 196)
        };
        // 用户标签颜色
        let user = if dark {
            Color::Rgb(162, 214, 255)
        } else {
            Color::Rgb(18, 92, 160)
        };
        // 工具标签颜色（金黄色调）
        let tool = if dark {
            Color::Rgb(225, 194, 104)
        } else {
            Color::Rgb(156, 108, 0)
        };

        Self {
            border: Style::default().fg(border),
            border_active: Style::default().fg(accent),
            user_label: Style::default().fg(user).add_modifier(Modifier::BOLD),
            assistant_label: Style::default().fg(accent).add_modifier(Modifier::BOLD),
            tool_label: Style::default().fg(tool).add_modifier(Modifier::BOLD),
            system_label: Style::default().fg(muted).add_modifier(Modifier::BOLD),
            error_label: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            body: Style::default().fg(if dark { Color::Gray } else { Color::Black }),
            dim: Style::default().fg(muted),
            status: Style::default().fg(accent),
            status_busy: Style::default().fg(tool).add_modifier(Modifier::BOLD),
            footer: Style::default().fg(muted),
            composer: Style::default().fg(if dark { Color::White } else { Color::Black }),
            composer_placeholder: Style::default().fg(muted),
            popup_border: Style::default().fg(accent),
            popup_selected: Style::default().fg(accent).add_modifier(Modifier::BOLD),
        }
    }
}

/// 判断终端是否为深色背景。
///
/// 目前仅识别 Apple_Terminal 为浅色终端，其他终端默认视为深色。
fn is_terminal_dark() -> bool {
    !matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("Apple_Terminal")
    )
}
