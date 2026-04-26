use std::{io, io::Write};

use ratatui::{
    backend::Backend,
    layout::{Offset, Rect, Size},
};

use crate::ui::{
    HistoryLine,
    custom_terminal::{Frame, Terminal},
    insert_history::insert_history_lines,
};

#[derive(Debug)]
pub struct TuiRuntime<B>
where
    B: Backend<Error = io::Error> + Write,
{
    terminal: Terminal<B>,
    pending_history_lines: Vec<HistoryLine>,
    deferred_history_lines: Vec<HistoryLine>,
    overlay_open: bool,
}

impl<B> TuiRuntime<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn with_backend(backend: B) -> io::Result<Self> {
        let terminal = Terminal::with_options(backend)?;
        Ok(Self {
            terminal,
            pending_history_lines: Vec::new(),
            deferred_history_lines: Vec::new(),
            overlay_open: false,
        })
    }

    pub fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<B> {
        &mut self.terminal
    }

    pub fn screen_size(&self) -> io::Result<Size> {
        self.terminal.size()
    }

    pub fn stage_history_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = HistoryLine>,
    {
        self.pending_history_lines.extend(lines);
    }

    pub fn draw<F>(&mut self, viewport_height: u16, overlay_open: bool, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>, Rect),
    {
        if let Some(area) = self.pending_viewport_area()? {
            self.terminal.set_viewport_area(area);
            self.terminal.clear()?;
        }

        let mut needs_full_repaint = self.update_inline_viewport(viewport_height)?;

        if overlay_open {
            if !self.pending_history_lines.is_empty() {
                self.deferred_history_lines
                    .append(&mut self.pending_history_lines);
            }
        } else {
            if self.overlay_open && !self.deferred_history_lines.is_empty() {
                self.pending_history_lines
                    .append(&mut self.deferred_history_lines);
            }
            needs_full_repaint |= self.flush_pending_history_lines()?;
        }
        self.overlay_open = overlay_open;

        if needs_full_repaint {
            self.terminal.invalidate_viewport();
        }

        self.terminal.draw(|frame| {
            let area = frame.area();
            render(frame, area);
        })
    }

    fn flush_pending_history_lines(&mut self) -> io::Result<bool> {
        if self.pending_history_lines.is_empty() {
            return Ok(false);
        }
        let lines = std::mem::take(&mut self.pending_history_lines);
        insert_history_lines(&mut self.terminal, lines)?;
        Ok(true)
    }

    fn update_inline_viewport(&mut self, height: u16) -> io::Result<bool> {
        let size = self.terminal.size()?;
        let mut area = self.terminal.viewport_area;
        area.height = height.min(size.height).max(1);
        area.width = size.width;

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            self.terminal
                .backend_mut()
                .scroll_region_up(0..area.top(), scroll_by)?;
            area.y = size.height.saturating_sub(area.height);
        }

        if area != self.terminal.viewport_area {
            self.terminal.clear()?;
            self.terminal.set_viewport_area(area);
            return Ok(true);
        }

        Ok(false)
    }

    fn pending_viewport_area(&mut self) -> io::Result<Option<Rect>> {
        let screen_size = self.terminal.size()?;
        let last_known_screen_size = self.terminal.last_known_screen_size;
        if screen_size != last_known_screen_size {
            let cursor_pos = self.terminal.get_cursor_position()?;
            let last_known_cursor_pos = self.terminal.last_known_cursor_pos;
            if cursor_pos.y != last_known_cursor_pos.y {
                let offset = Offset {
                    x: 0,
                    y: cursor_pos.y as i32 - last_known_cursor_pos.y as i32,
                };
                return Ok(Some(self.terminal.viewport_area.offset(offset)));
            }
        }
        Ok(None)
    }
}
