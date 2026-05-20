//! Slash command palette overlay component.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Margin, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{Component, KeyOutcome};
use crate::tui::{
    command::slash::{SlashCommandSpec, filtered},
    theme::Theme,
};

pub struct SlashPalette {
    pub filter: String,
    pub selected: usize,
    pub extension_commands: Vec<SlashCommandSpec>,
    theme: Theme,
}

impl SlashPalette {
    pub fn new(theme: Theme) -> Self {
        Self {
            filter: String::new(),
            selected: 0,
            extension_commands: Vec::new(),
            theme,
        }
    }

    pub fn commands(&self) -> Vec<SlashCommandSpec> {
        filtered(&self.filter, &self.extension_commands)
    }

    pub fn move_up(&mut self) {
        let len = self.commands().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = len - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let len = self.commands().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn selected_command(&self) -> Option<SlashCommandSpec> {
        let commands = self.commands();
        commands
            .get(self.selected.min(commands.len().saturating_sub(1)))
            .cloned()
    }
}

impl Component for SlashPalette {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let commands = self.commands();
        if commands.is_empty() {
            return;
        }
        let max_height = area.height.saturating_sub(1).max(1);
        let visible = commands
            .len()
            .min(max_height.saturating_sub(2).max(1) as usize);
        let selected = self.selected.min(commands.len().saturating_sub(1));
        let start = selected.saturating_add(1).saturating_sub(visible);
        let height = (visible as u16 + 2).min(max_height);
        let popup = bottom_popup_rect(area, 70, height);
        let inner = popup.inner(Margin {
            vertical: 1,
            horizontal: 1,
        });
        let lines: Vec<Line> = commands
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
            .map(|(idx, cmd)| {
                let is_sel = start + idx == selected;
                let label_style = if is_sel {
                    self.theme.popup_selected
                } else {
                    self.theme.assistant_label
                };
                let desc_style = if is_sel {
                    self.theme.body
                } else {
                    self.theme.dim
                };
                Line::from(vec![
                    Span::styled(format!("{:<16}", cmd.usage), label_style),
                    Span::styled(cmd.description.clone(), desc_style),
                ])
            })
            .collect();
        Clear.render(popup, buf);
        Block::default()
            .borders(Borders::ALL)
            .border_style(self.theme.popup_border)
            .title(" Slash Commands ")
            .render(popup, buf);
        Paragraph::new(Text::from(lines)).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        0 // Overlay — doesn't consume vertical space in the main layout
    }

    fn handle_key(&mut self, key: &KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Up => {
                self.move_up();
                KeyOutcome::Handled
            },
            KeyCode::Down => {
                self.move_down();
                KeyOutcome::Handled
            },
            _ => KeyOutcome::NotHandled,
        }
    }
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
