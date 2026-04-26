use std::{io, io::Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Colors, Print, SetAttribute, SetBackgroundColor, SetColors, SetForegroundColor},
    terminal::Clear,
};
use ratatui::{
    backend::{Backend, ClearType},
    buffer::Buffer,
    layout::{Position, Rect, Size},
    style::{Color, Modifier},
    widgets::Widget,
};
use unicode_width::UnicodeWidthStr;

fn display_width(s: &str) -> usize {
    if !s.contains('\x1B') {
        return s.width();
    }

    let mut visible = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1B' && chars.clone().next() == Some(']') {
            chars.next();
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
            }
            continue;
        }
        visible.push(ch);
    }
    visible.width()
}

#[derive(Debug)]
pub struct Frame<'a> {
    pub(crate) cursor_position: Option<Position>,
    pub(crate) viewport_area: Rect,
    pub(crate) buffer: &'a mut Buffer,
}

impl Frame<'_> {
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }
}

#[derive(Debug)]
pub struct Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    hidden_cursor: bool,
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    pub last_known_cursor_pos: Position,
    visible_history_rows: u16,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    fn drop(&mut self) {
        let _ = self.show_cursor();
    }
}

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend
            .get_cursor_position()
            .unwrap_or(Position { x: 0, y: 0 });
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
        })
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn set_viewport_area(&mut self, area: Rect) {
        self.current_buffer_mut().resize(area);
        self.previous_buffer_mut().resize(area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
    }

    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.backend
            .set_cursor_position(self.viewport_area.as_position())?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    pub fn invalidate_viewport(&mut self) {
        self.previous_buffer_mut().reset();
    }

    pub fn visible_history_rows(&self) -> u16 {
        self.visible_history_rows
    }

    pub fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(inserted_rows)
            .min(self.viewport_area.top());
    }

    pub fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.size()?;
        if screen_size != self.last_known_screen_size {
            self.last_known_screen_size = screen_size;
        }
        Ok(())
    }

    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.autoresize()?;

        let mut frame = Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        };
        render_callback(&mut frame);
        let cursor_position = frame.cursor_position;

        self.flush()?;
        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            },
        }
        self.swap_buffers();
        Backend::flush(&mut self.backend)?;
        Ok(())
    }

    fn current_buffer(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    fn previous_buffer(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    fn flush(&mut self) -> io::Result<()> {
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        let mut last_put = None;
        for command in &updates {
            if let DrawCommand::Put { x, y, .. } = command {
                last_put = Some(Position { x: *x, y: *y });
            }
        }
        if let Some(position) = last_put {
            self.last_known_cursor_pos = position;
        }
        draw_commands(&mut self.backend, updates.into_iter())
    }

    fn swap_buffers(&mut self) {
        self.previous_buffer_mut().reset();
        self.current = 1 - self.current;
    }
}

#[derive(Debug)]
enum DrawCommand {
    Put {
        x: u16,
        y: u16,
        cell: ratatui::buffer::Cell,
    },
    ClearToEnd {
        x: u16,
        y: u16,
        bg: Color,
    },
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let previous_buffer = &a.content;
    let next_buffer = &b.content;

    let mut updates = Vec::new();
    let mut last_nonblank_columns = vec![0; a.area.height as usize];
    for y in 0..a.area.height {
        let row_start = y as usize * a.area.width as usize;
        let row_end = row_start + a.area.width as usize;
        let row = &next_buffer[row_start..row_end];
        let bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        let mut last_nonblank_column = 0usize;
        let mut column = 0usize;
        while column < row.len() {
            let cell = &row[column];
            let width = display_width(cell.symbol());
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last_nonblank_column = column + width.saturating_sub(1);
            }
            column += width.max(1);
        }

        if last_nonblank_column + 1 < row.len() {
            let (x, y) = a.pos_of(row_start + last_nonblank_column + 1);
            updates.push(DrawCommand::ClearToEnd { x, y, bg });
        }
        last_nonblank_columns[y as usize] = last_nonblank_column as u16;
    }

    let mut invalidated = 0usize;
    let mut to_skip = 0usize;
    for (index, (current, previous)) in next_buffer.iter().zip(previous_buffer.iter()).enumerate() {
        if !current.skip && (current != previous || invalidated > 0) && to_skip == 0 {
            let (x, y) = a.pos_of(index);
            let row = index / a.area.width as usize;
            if x <= last_nonblank_columns[row] {
                updates.push(DrawCommand::Put {
                    x,
                    y,
                    cell: current.clone(),
                });
            }
        }
        to_skip = display_width(current.symbol()).saturating_sub(1);
        let affected_width = display_width(current.symbol()).max(display_width(previous.symbol()));
        invalidated = affected_width.max(invalidated).saturating_sub(1);
    }
    updates
}

fn draw_commands<I>(writer: &mut impl Write, commands: I) -> io::Result<()>
where
    I: Iterator<Item = DrawCommand>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<Position> = None;

    for command in commands {
        let (x, y) = match &command {
            DrawCommand::Put { x, y, .. } | DrawCommand::ClearToEnd { x, y, .. } => (*x, *y),
        };
        if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(writer, MoveTo(x, y))?;
        }
        last_pos = Some(Position { x, y });
        match command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    let diff = ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    };
                    diff.queue(writer)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(
                        writer,
                        SetColors(Colors::new(
                            to_crossterm_color(cell.fg),
                            to_crossterm_color(cell.bg)
                        ))
                    )?;
                    fg = cell.fg;
                    bg = cell.bg;
                }
                queue!(writer, Print(cell.symbol()))?;
            },
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
                modifier = Modifier::empty();
                queue!(writer, SetBackgroundColor(to_crossterm_color(clear_bg)))?;
                bg = clear_bg;
                queue!(writer, Clear(crossterm::terminal::ClearType::UntilNewLine))?;
            },
        }
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;
    Ok(())
}

fn to_crossterm_color(color: Color) -> crossterm::style::Color {
    match color {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::DarkRed,
        Color::Green => crossterm::style::Color::DarkGreen,
        Color::Yellow => crossterm::style::Color::DarkYellow,
        Color::Blue => crossterm::style::Color::DarkBlue,
        Color::Magenta => crossterm::style::Color::DarkMagenta,
        Color::Cyan => crossterm::style::Color::DarkCyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::Red,
        Color::LightGreen => crossterm::style::Color::Green,
        Color::LightYellow => crossterm::style::Color::Yellow,
        Color::LightBlue => crossterm::style::Color::Blue,
        Color::LightMagenta => crossterm::style::Color::Magenta,
        Color::LightCyan => crossterm::style::Color::Cyan,
        Color::White => crossterm::style::Color::White,
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
        Color::Indexed(index) => crossterm::style::Color::AnsiValue(index),
    }
}

struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, writer: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(writer, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(writer, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(CAttribute::RapidBlink))?;
        }
        Ok(())
    }
}
