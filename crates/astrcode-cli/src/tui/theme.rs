//! Claude Code 风格终端主题：低噪音文本、轻量工具状态和克制强调色。
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
        // 次要文本颜色保持低饱和，避免抢正文注意力。
        let muted = if dark {
            Color::Rgb(124, 124, 118)
        } else {
            Color::Rgb(92, 92, 86)
        };
        let border = if dark {
            Color::Rgb(58, 58, 54)
        } else {
            Color::Rgb(198, 198, 188)
        };
        let accent = if dark {
            Color::Rgb(192, 169, 112)
        } else {
            Color::Rgb(126, 92, 35)
        };
        let user = if dark {
            Color::Rgb(156, 190, 150)
        } else {
            Color::Rgb(45, 118, 76)
        };
        let tool = if dark {
            Color::Rgb(176, 160, 118)
        } else {
            Color::Rgb(118, 94, 46)
        };

        Self {
            border: Style::default().fg(border),
            border_active: Style::default().fg(accent),
            user_label: Style::default().fg(user).add_modifier(Modifier::BOLD),
            assistant_label: Style::default().fg(accent).add_modifier(Modifier::BOLD),
            tool_label: Style::default().fg(tool).add_modifier(Modifier::BOLD),
            system_label: Style::default().fg(muted).add_modifier(Modifier::BOLD),
            error_label: Style::default()
                .fg(if dark {
                    Color::Rgb(218, 112, 112)
                } else {
                    Color::Rgb(170, 40, 40)
                })
                .add_modifier(Modifier::BOLD),
            body: Style::default().fg(if dark {
                Color::Rgb(205, 205, 196)
            } else {
                Color::Rgb(35, 35, 32)
            }),
            dim: Style::default().fg(muted),
            status: Style::default().fg(accent),
            status_busy: Style::default().fg(tool).add_modifier(Modifier::BOLD),
            footer: Style::default().fg(muted),
            composer: Style::default().fg(if dark {
                Color::Rgb(235, 235, 224)
            } else {
                Color::Rgb(24, 24, 22)
            }),
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
