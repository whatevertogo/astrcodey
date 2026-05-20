//! TerminalSession: raw mode, CSI 2026, inline viewport, resize heuristic.
//!
//! Thin wrapper around the existing custom_terminal + insert_history infrastructure.

use std::io::{self, Stdout};

use crossterm::{
    SynchronizedUpdate,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, layout::Position};

use crate::tui::{
    custom_terminal::Terminal as CustomTerminal, insert_history::insert_history_lines,
    render::scrollback_entry_to_lines, store::transcript::ScrollbackEntry, theme::Theme,
};

const INLINE_VIEWPORT_HEIGHT: u16 = 4;

pub struct TerminalSession {
    pub terminal: CustomTerminal<CrosstermBackend<Stdout>>,
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
        Ok(Self { terminal })
    }

    pub fn composer_width(&self) -> usize {
        self.terminal.composer_width()
    }

    /// Flush scrollback entries into terminal native scrollback.
    pub fn flush_scrollback(
        &mut self,
        entries: Vec<ScrollbackEntry>,
        theme: &Theme,
    ) -> io::Result<()> {
        for entry in entries {
            let width = self.terminal.viewport_area.width;
            let lines = scrollback_entry_to_lines(&entry, width, theme);
            insert_history_lines(&mut self.terminal, lines)?;
        }
        Ok(())
    }

    /// Draw the bottom inline viewport.
    pub fn draw_frame<F>(&mut self, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut crate::tui::custom_terminal::Frame<'_>),
    {
        let pending_viewport_area = self.pending_viewport_area()?;
        let _ = io::stdout().sync_update(|_| {
            if let Some(new_area) = pending_viewport_area {
                self.terminal.set_viewport_area(new_area);
                self.terminal.clear()?;
            }
            let needs_full_repaint = self
                .terminal
                .update_inline_viewport(INLINE_VIEWPORT_HEIGHT)?;
            if needs_full_repaint {
                self.terminal.invalidate_viewport();
            }
            self.terminal.draw(render_fn)
        })?;
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
