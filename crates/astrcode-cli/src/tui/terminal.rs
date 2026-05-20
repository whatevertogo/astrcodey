//! TerminalSession with transcript reflow (codex-rs design).
//!
//! Wraps custom_terminal + insert_history with an in-memory history buffer.
//! On resize: clear visible history rows, re-insert from buffer for the new width.
//! This prevents "swallowed lines" on terminal resize.

use std::io::{self, Stdout, Write};

use crossterm::{
    SynchronizedUpdate,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, layout::Position, text::Line};

use crate::tui::{
    custom_terminal::Terminal as CustomTerminal, insert_history::insert_history_lines,
    render::scrollback_entry_to_lines, store::transcript::ScrollbackEntry, theme::Theme,
};

const INLINE_VIEWPORT_HEIGHT: u16 = 4;
/// Maximum number of history lines to replay on resize (prevents lag on huge sessions).
const REFLOW_MAX_LINES: usize = 500;

pub struct TerminalSession {
    pub terminal: CustomTerminal<CrosstermBackend<Stdout>>,
    /// All history lines ever written, kept for resize reflow.
    history_source: Vec<Line<'static>>,
    /// Last known terminal width (to detect width changes that need reflow).
    last_width: u16,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste)?;

        #[cfg(unix)]
        let backend = CrosstermBackend::new(stdout);
        #[cfg(not(unix))]
        let mut backend = CrosstermBackend::new(stdout);

        #[cfg(unix)]
        let cursor_pos = match crate::tui::terminal_probe::cursor_position(
            crate::tui::terminal_probe::DEFAULT_TIMEOUT,
        ) {
            Ok(Some(pos)) => pos,
            _ => Position { x: 0, y: 0 },
        };

        #[cfg(not(unix))]
        let cursor_pos = backend
            .get_cursor_position()
            .unwrap_or(Position { x: 0, y: 0 });

        let terminal = CustomTerminal::with_options_and_cursor_position(backend, cursor_pos)?;
        let last_width = terminal.viewport_area.width;

        Ok(Self {
            terminal,
            history_source: Vec::new(),
            last_width,
        })
    }

    pub fn composer_width(&self) -> usize {
        self.terminal.composer_width()
    }

    /// Flush scrollback entries. Stores lines in history_source for reflow.
    pub fn flush_scrollback(
        &mut self,
        entries: Vec<ScrollbackEntry>,
        theme: &Theme,
    ) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let width = self.terminal.viewport_area.width;
        for entry in entries {
            let lines = scrollback_entry_to_lines(&entry, width, theme);
            // Store in memory for reflow.
            self.history_source.extend(lines.clone());
            // Insert into terminal scrollback.
            insert_history_lines(&mut self.terminal, lines)?;
        }
        Ok(())
    }

    /// Draw the bottom inline viewport. Handles resize + reflow.
    pub fn draw_frame_with_height<F>(&mut self, height: u16, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut crate::tui::custom_terminal::Frame<'_>),
    {
        let screen_size = self.terminal.size()?;
        let width_changed = screen_size.width != self.last_width;

        if width_changed {
            self.last_width = screen_size.width;
            self.reflow_history(screen_size)?;
        } else {
            let pending = self.pending_viewport_area()?;
            if let Some(new_area) = pending {
                self.terminal.set_viewport_area(new_area);
                self.terminal.clear()?;
            }
        }

        let _ = io::stdout().sync_update(|_| {
            let needs_full_repaint = self.terminal.update_inline_viewport(height)?;
            if needs_full_repaint {
                self.terminal.invalidate_viewport();
            }
            self.terminal.draw(render_fn)
        })?;
        Ok(())
    }

    /// Draw the bottom inline viewport. Handles resize + reflow.
    pub fn draw_frame<F>(&mut self, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut crate::tui::custom_terminal::Frame<'_>),
    {
        self.draw_frame_with_height(INLINE_VIEWPORT_HEIGHT, render_fn)
    }

    /// Clear all visible history and re-insert from history_source at the new width.
    fn reflow_history(&mut self, screen_size: ratatui::layout::Size) -> io::Result<()> {
        // Reset viewport to top of screen (as if starting fresh).
        let new_area = ratatui::layout::Rect::new(0, 0, screen_size.width, 0);
        self.terminal.set_viewport_area(new_area);

        // Clear BOTH visible screen AND scrollback buffer.
        // CSI 2J = clear visible screen, CSI 3J = purge scrollback, CSI H = home cursor.
        let writer = self.terminal.backend_mut();
        std::io::Write::write_all(writer, b"\x1b[2J\x1b[3J\x1b[H")?;
        std::io::Write::flush(writer)?;
        self.terminal.last_known_screen_size = screen_size;
        self.terminal.last_known_cursor_pos = Position { x: 0, y: 0 };
        self.terminal.invalidate_viewport();

        // Re-insert the tail of history that fits on screen.
        let available_rows = screen_size.height.saturating_sub(INLINE_VIEWPORT_HEIGHT) as usize;
        let replay_count = self
            .history_source
            .len()
            .min(available_rows)
            .min(REFLOW_MAX_LINES);
        let start = self.history_source.len().saturating_sub(replay_count);
        let lines_to_replay: Vec<Line<'static>> = self.history_source[start..].to_vec();

        if !lines_to_replay.is_empty() {
            insert_history_lines(&mut self.terminal, lines_to_replay)?;
        }

        Ok(())
    }

    fn pending_viewport_area(&mut self) -> io::Result<Option<ratatui::layout::Rect>> {
        let screen_size = self.terminal.size()?;
        let last_known = self.terminal.last_known_screen_size;
        if screen_size != last_known {
            if let Ok(cursor_pos) = self.terminal.get_cursor_position() {
                let last_cursor = self.terminal.last_known_cursor_pos;
                if cursor_pos.y != last_cursor.y {
                    let offset = ratatui::layout::Offset {
                        x: 0,
                        y: cursor_pos.y as i32 - last_cursor.y as i32,
                    };
                    return Ok(Some(self.terminal.viewport_area.offset(offset)));
                }
            }
        }
        Ok(None)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}
