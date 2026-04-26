//! Codex-inspired terminal theme: quiet transcript, focused composer, clear status accents.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub border: Style,
    pub border_active: Style,
    pub user_label: Style,
    pub assistant_label: Style,
    pub tool_label: Style,
    pub system_label: Style,
    pub error_label: Style,
    pub body: Style,
    pub dim: Style,
    pub status: Style,
    pub status_busy: Style,
    pub footer: Style,
    pub composer: Style,
    pub composer_placeholder: Style,
    pub popup_border: Style,
    pub popup_selected: Style,
}

impl Theme {
    pub fn detect() -> Self {
        let dark = is_terminal_dark();
        let muted = if dark {
            Color::Rgb(120, 129, 148)
        } else {
            Color::Rgb(96, 103, 120)
        };
        let border = if dark {
            Color::Rgb(54, 62, 79)
        } else {
            Color::Rgb(196, 204, 222)
        };
        let accent = if dark {
            Color::Rgb(112, 197, 255)
        } else {
            Color::Rgb(0, 120, 196)
        };
        let user = if dark {
            Color::Rgb(162, 214, 255)
        } else {
            Color::Rgb(18, 92, 160)
        };
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

fn is_terminal_dark() -> bool {
    !matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("Apple_Terminal")
    )
}
