//! Terminal session using DECSTBM scroll margin to pin the bottom panel.
//!
//! Design:
//! - NO alternate screen (user keeps native scrollback + scroll wheel)
//! - Set scroll region to [0, rows - PANEL_HEIGHT) so history scrolls natively
//! - Bottom panel is OUTSIDE the scroll region — never pushed into scrollback
//! - History lines written inside scroll region → terminal scrolls them naturally
//! - On resize: reset scroll region to new size, redraw panel
//!
//! This is how codex-cli and claude-code work: user can scroll up with mouse/keyboard.

use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{Print, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    style::{Color, Modifier},
    text::Line,
};

use crate::tui::{
    render::scrollback_entry_to_lines, store::transcript::ScrollbackEntry, theme::Theme,
};

/// Fixed height of the bottom panel.
const PANEL_HEIGHT: u16 = 4;

pub struct TerminalSession {
    stdout: Stdout,
    size: (u16, u16),
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnableBracketedPaste)?;
        let size = terminal::size()?;

        // Set up: move to bottom, reserve panel space, set scroll region.
        // First, scroll screen up to make room for the panel at bottom.
        // Then set scroll region to exclude the panel rows.
        Self::setup_scroll_region(&mut stdout, size)?;

        Ok(Self { stdout, size })
    }

    fn setup_scroll_region(stdout: &mut Stdout, size: (u16, u16)) -> io::Result<()> {
        let scroll_bottom = size.1.saturating_sub(PANEL_HEIGHT);
        // DECSTBM: set scroll region to rows 1..scroll_bottom (1-indexed for VT100).
        // This means rows [0, scroll_bottom) can scroll; rows [scroll_bottom, size.1) are fixed.
        write!(stdout, "\x1b[1;{}r", scroll_bottom)?;
        // Move cursor to the last row of the scroll region (where new history will be written).
        execute!(stdout, MoveTo(0, scroll_bottom.saturating_sub(1)))?;
        stdout.flush()?;
        Ok(())
    }

    pub fn composer_width(&self) -> usize {
        self.size.0.saturating_sub(4).max(1) as usize
    }

    /// Write scrollback entries into the scroll region. The terminal will
    /// naturally scroll old lines into native scrollback (accessible via scroll wheel).
    pub fn flush_scrollback(
        &mut self,
        entries: Vec<ScrollbackEntry>,
        theme: &Theme,
    ) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.size = terminal::size()?;
        let width = self.size.0;
        let scroll_bottom = self.size.1.saturating_sub(PANEL_HEIGHT);

        // Position cursor at the bottom of the scroll region.
        queue!(self.stdout, MoveTo(0, scroll_bottom.saturating_sub(1)))?;

        for entry in entries {
            let lines = scrollback_entry_to_lines(&entry, width, theme);
            for line in lines {
                // Print newline first to scroll existing content up, then write the line.
                queue!(self.stdout, Print("\n"))?;
                queue!(self.stdout, MoveTo(0, scroll_bottom.saturating_sub(1)))?;
                self.write_line(&line)?;
            }
        }
        self.stdout.flush()?;
        Ok(())
    }

    /// Redraw the bottom panel (fixed area below scroll region).
    pub fn draw_panel(
        &mut self,
        panel_lines: Vec<Line<'static>>,
        cursor_col: u16,
        cursor_row_offset: u16,
    ) -> io::Result<()> {
        self.size = terminal::size()?;
        let panel_top = self.size.1.saturating_sub(PANEL_HEIGHT);

        // Also re-establish scroll region in case terminal was resized.
        write!(self.stdout, "\x1b[1;{}r", panel_top)?;

        queue!(self.stdout, Hide)?;

        // Clear and draw each panel line.
        for i in 0..PANEL_HEIGHT {
            queue!(self.stdout, MoveTo(0, panel_top + i))?;
            queue!(self.stdout, Clear(ClearType::CurrentLine))?;
            if let Some(line) = panel_lines.get(i as usize) {
                self.write_line(line)?;
            }
        }

        // Position cursor in composer.
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
        // Reset scroll region to full screen.
        let _ = write!(self.stdout, "\x1b[r");
        let _ = execute!(self.stdout, Show);
        // Move cursor below the panel area so shell prompt appears cleanly.
        if let Ok(size) = terminal::size() {
            let _ = execute!(self.stdout, MoveTo(0, size.1.saturating_sub(1)));
        }
        let _ = execute!(self.stdout, Print("\n"));
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
