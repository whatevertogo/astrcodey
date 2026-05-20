//! Footer component — single-line status bar at the bottom.

use astrcode_support::text::compact_inline;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::{Component, KeyOutcome};
use crate::tui::theme::Theme;

pub struct Footer {
    pub status: String,
    pub model_name: String,
    pub working_dir: String,
    pub active_session_id: Option<String>,
    pub is_streaming: bool,
    theme: Theme,
}

impl Footer {
    pub fn new(theme: Theme) -> Self {
        Self {
            status: "Ready".into(),
            model_name: String::new(),
            working_dir: String::new(),
            active_session_id: None,
            is_streaming: false,
            theme,
        }
    }

    fn footer_text(&self) -> String {
        let session = self
            .active_session_id
            .as_deref()
            .map(|id| id.get(..8).unwrap_or(id))
            .unwrap_or("none");
        let model = if self.model_name.is_empty() {
            "model: pending".to_string()
        } else {
            self.model_name.clone()
        };
        let cwd = if self.working_dir.is_empty() {
            "cwd pending".into()
        } else {
            compact_path(&self.working_dir)
        };
        let hints = if self.is_streaming {
            "Esc stop"
        } else {
            "Enter send · Shift+Enter newline · /help"
        };
        format!("  {model} · {cwd} · session {session}   {hints}")
    }
}

impl Component for Footer {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let text = fit_line(&self.footer_text(), area.width as usize);
        Paragraph::new(Line::from(Span::styled(text, self.theme.footer))).render(area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

fn compact_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<_> = normalized.split('/').filter(|p| !p.is_empty()).collect();
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
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let text_w: usize = text
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    if text_w <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}
