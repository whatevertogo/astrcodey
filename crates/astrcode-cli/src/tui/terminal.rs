//! Simple TUI terminal: uses alternate screen + full redraw each frame.
//!
//! This is the simplest correct approach:
//! - Enter alternate screen on start (preserves user's shell scrollback)
//! - Each frame: clear screen, render all visible content (history tail + panel)
//! - History is stored in memory; we show the last N lines that fit
//! - Resize: just redraw (no DECSTBM, no scroll regions, no reflow needed)
//! - On exit: leave alternate screen (user's original scrollback restored)

use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{Print, SetAttribute, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    style::{Color, Modifier},
    text::Line,
};

use crate::tui::{
    render::scrollback_entry_to_lines, store::transcript::ScrollbackEntry, theme::Theme,
};

/// Fixed height of the bottom panel (composer + status + footer + separator).
const PANEL_HEIGHT: u16 = 4;

pub struct TerminalSession {
    stdout: Stdout,
    /// All rendered history lines (kept in memory for redraw on resize).
    history: Vec<Line<'static>>,
    /// Scroll offset from the bottom (0 = show latest).
    scroll_offset: usize,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnableBracketedPaste, EnterAlternateScreen, Hide)?;
        Ok(Self {
            stdout,
            history: Vec::new(),
            scroll_offset: 0,
        })
    }

    pub fn composer_width(&self) -> usize {
        let (cols, _) = terminal::size().unwrap_or((80, 24));
        cols.saturating_sub(4).max(1) as usize
    }

    /// Add scrollback entries to history buffer.
    pub fn flush_scrollback(
        &mut self,
        entries: Vec<ScrollbackEntry>,
        theme: &Theme,
    ) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let (cols, _) = terminal::size().unwrap_or((80, 24));
        for entry in entries {
            let lines = scrollback_entry_to_lines(&entry, cols, theme);
            self.history.extend(lines);
        }
        // Auto-scroll to bottom when new content arrives.
        self.scroll_offset = 0;
        Ok(())
    }

    /// Full redraw: history area (top) + panel (bottom).
    pub fn draw_frame(
        &mut self,
        panel_lines: Vec<Line<'static>>,
        cursor_col: u16,
        cursor_row_offset: u16,
    ) -> io::Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let history_rows = rows.saturating_sub(PANEL_HEIGHT) as usize;
        let panel_top = rows.saturating_sub(PANEL_HEIGHT);

        // Start drawing — hide cursor to avoid flicker.
        queue!(self.stdout, Hide)?;
        queue!(self.stdout, MoveTo(0, 0))?;
        queue!(self.stdout, Clear(ClearType::All))?;

        // Draw history (last N lines that fit).
        let total_history = self.history.len();
        let start = total_history.saturating_sub(history_rows + self.scroll_offset);
        let end = total_history.saturating_sub(self.scroll_offset);
        let visible_lines: Vec<_> = self.history[start..end].to_vec();

        for (i, line) in visible_lines.iter().enumerate() {
            if i >= history_rows {
                break;
            }
            queue!(self.stdout, MoveTo(0, i as u16))?;
            self.write_line(line)?;
        }

        // Draw separator line.
        queue!(self.stdout, MoveTo(0, panel_top.saturating_sub(1)))?;
        queue!(
            self.stdout,
            SetForegroundColor(crossterm::style::Color::DarkGrey)
        )?;
        let sep: String = "─".repeat(cols as usize);
        queue!(self.stdout, Print(&sep))?;
        queue!(
            self.stdout,
            SetAttribute(crossterm::style::Attribute::Reset)
        )?;

        // Draw panel lines.
        for (i, line) in panel_lines.iter().enumerate() {
            if i as u16 >= PANEL_HEIGHT {
                break;
            }
            queue!(self.stdout, MoveTo(0, panel_top + i as u16))?;
            self.write_line(line)?;
        }

        // Show cursor at composer position.
        let cursor_y = panel_top + cursor_row_offset;
        queue!(self.stdout, Show)?;
        execute!(self.stdout, MoveTo(cursor_col, cursor_y))?;
        Ok(())
    }

    fn write_line(&mut self, line: &Line<'_>) -> io::Result<()> {
        for span in &line.spans {
            if let Some(fg) = span.style.fg {
                queue!(self.stdout, SetForegroundColor(ratatui_to_crossterm(fg)))?;
            }
            if span.style.add_modifier.contains(Modifier::BOLD) {
                queue!(self.stdout, SetAttribute(crossterm::style::Attribute::Bold))?;
            }
            if span.style.add_modifier.contains(Modifier::DIM) {
                queue!(self.stdout, SetAttribute(crossterm::style::Attribute::Dim))?;
            }
            if span.style.add_modifier.contains(Modifier::ITALIC) {
                queue!(
                    self.stdout,
                    SetAttribute(crossterm::style::Attribute::Italic)
                )?;
            }
            queue!(self.stdout, Print(&*span.content))?;
            queue!(
                self.stdout,
                SetAttribute(crossterm::style::Attribute::Reset)
            )?;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen);
        let _ = execute!(self.stdout, DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

fn ratatui_to_crossterm(c: Color) -> crossterm::style::Color {
    match c {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::Red,
        Color::Green => crossterm::style::Color::Green,
        Color::Yellow => crossterm::style::Color::Yellow,
        Color::Blue => crossterm::style::Color::Blue,
        Color::Magenta => crossterm::style::Color::Magenta,
        Color::Cyan => crossterm::style::Color::Cyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::DarkRed,
        Color::LightGreen => crossterm::style::Color::DarkGreen,
        Color::LightYellow => crossterm::style::Color::DarkYellow,
        Color::LightBlue => crossterm::style::Color::DarkBlue,
        Color::LightMagenta => crossterm::style::Color::DarkMagenta,
        Color::LightCyan => crossterm::style::Color::DarkCyan,
        Color::White => crossterm::style::Color::White,
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
        Color::Indexed(i) => crossterm::style::Color::AnsiValue(i),
    }
}
