//! Inserts finalized history rows into terminal scrollback.
//!
//! Codex uses the terminal scrollback itself for finalized chat history, so inserting a history
//! cell is an escape-sequence operation rather than a normal ratatui render.

use std::{fmt, io, io::Write};

use crossterm::{
    Command,
    cursor::{MoveDown, MoveTo, MoveToColumn, RestorePosition, SavePosition},
    queue,
    style::{
        Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
        SetForegroundColor,
    },
    terminal::{Clear, ClearType},
};
use ratatui::{
    layout::Size,
    style::{Color, Modifier},
    text::{Line, Span},
};
use ratatui_crossterm::IntoCrossterm;

use super::custom_terminal::Terminal;

/// Insert `lines` above the viewport using DECSTBM scroll regions and reverse index.
pub fn insert_history_lines<B>(terminal: &mut Terminal<B>, lines: Vec<Line>) -> io::Result<()>
where
    B: ratatui::backend::Backend<Error = io::Error> + Write,
{
    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));

    let mut area = terminal.viewport_area;
    let mut should_update_area = false;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let writer = terminal.backend_mut();

    // Pre-wrap lines for terminal scrollback
    let wrap_width = area.width.max(1) as usize;
    let mut wrapped = Vec::new();
    let mut wrapped_rows = 0usize;

    for line in &lines {
        let line_wrapped = adaptive_wrap_line(line, wrap_width);
        wrapped_rows += line_wrapped
            .iter()
            .map(|wrapped_line| wrapped_line.width().max(1).div_ceil(wrap_width))
            .sum::<usize>();
        wrapped.extend(line_wrapped);
    }
    let wrapped_lines = wrapped_rows as u16;

    let cursor_top = if area.bottom() < screen_size.height {
        let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());

        let top_1based = area.top() + 1;
        queue!(writer, SetScrollRegion(top_1based..screen_size.height))?;
        queue!(writer, MoveTo(/* x */ 0, area.top()))?;
        for _ in 0..scroll_amount {
            queue!(writer, Print("\x1bM"))?;
        }
        queue!(writer, ResetScrollRegion)?;

        let cursor_top = area.top().saturating_sub(1);
        area.y += scroll_amount;
        should_update_area = true;
        cursor_top
    } else {
        area.top().saturating_sub(1)
    };

    queue!(writer, SetScrollRegion(1..area.top()))?;
    queue!(writer, MoveTo(/* x */ 0, cursor_top))?;

    for line in &wrapped {
        queue!(writer, Print("\r\n"))?;
        write_history_line(writer, line, wrap_width)?;
    }

    queue!(writer, ResetScrollRegion)?;

    // Restore the cursor position to where it was before we started.
    queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;

    let _ = writer;
    if should_update_area {
        terminal.set_viewport_area(area);
    }
    if wrapped_lines > 0 {
        terminal.note_history_rows_inserted(wrapped_lines);
    }

    Ok(())
}

/// Adaptive wrap line for scrollback (simplified version).
fn adaptive_wrap_line(line: &Line, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;

    if width == 0 || line.width() <= width {
        // 手动克隆：将 'a Line<'_> 转换为 Line<'static>
        let mut cloned_spans = Vec::new();
        for s in &line.spans {
            cloned_spans.push(Span {
                style: s.style,
                content: s.content.to_string().into(),
            });
        }
        return vec![Line {
            style: line.style,
            alignment: line.alignment,
            spans: cloned_spans,
        }];
    }

    let mut result = Vec::new();
    let mut current_line = Vec::new();
    let mut current_width = 0;

    for span in &line.spans {
        let content = &span.content;
        let style = span.style;

        // 逐字符处理
        for ch in content.chars() {
            let ch_width = ch.width().unwrap_or(1);

            if current_width + ch_width > width && !current_line.is_empty() {
                result.push(Line {
                    style: line.style,
                    alignment: None,
                    spans: current_line.clone(),
                });
                current_line.clear();
                current_width = 0;
            }

            current_width += ch_width;
            current_line.push(Span {
                style,
                content: ch.to_string().into(),
            });
        }
    }

    if !current_line.is_empty() {
        result.push(Line {
            style: line.style,
            alignment: None,
            spans: current_line,
        });
    }

    if result.is_empty() {
        result.push(Line {
            style: ratatui::style::Style::default(),
            alignment: None,
            spans: vec![],
        });
    }

    result
}

/// Render a single wrapped history line: clear continuation rows for wide lines,
/// set foreground/background colors, and write styled spans.
fn write_history_line<W: Write>(writer: &mut W, line: &Line, wrap_width: usize) -> io::Result<()> {
    let physical_rows = line.width().max(1).div_ceil(wrap_width) as u16;
    if physical_rows > 1 {
        queue!(writer, SavePosition)?;
        for _ in 1..physical_rows {
            queue!(writer, MoveDown(1), MoveToColumn(0))?;
            queue!(writer, Clear(ClearType::UntilNewLine))?;
        }
        queue!(writer, RestorePosition)?;
    }
    queue!(
        writer,
        SetColors(Colors::new(
            line.style
                .fg
                .map(Color::into_crossterm)
                .unwrap_or(CColor::Reset),
            line.style
                .bg
                .map(Color::into_crossterm)
                .unwrap_or(CColor::Reset)
        ))
    )?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    // Merge line-level style into each span so that ANSI colors reflect
    // line styles (e.g., blockquotes with green fg).
    let merged_spans: Vec<Span> = line
        .spans
        .iter()
        .map(|s| Span {
            style: s.style.patch(line.style),
            content: s.content.clone(),
        })
        .collect();
    write_spans(writer, merged_spans.iter())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute SetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute ResetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W>(self, mut w: W) -> io::Result<()>
    where
        W: io::Write,
    {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

fn write_spans<'a, I>(mut writer: &mut impl Write, content: I) -> io::Result<()>
where
    I: IntoIterator<Item = &'a Span<'a>>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut last_modifier = Modifier::empty();
    for span in content {
        let mut modifier = Modifier::empty();
        modifier.insert(span.style.add_modifier);
        modifier.remove(span.style.sub_modifier);
        if modifier != last_modifier {
            let diff = ModifierDiff {
                from: last_modifier,
                to: modifier,
            };
            diff.queue(&mut writer)?;
            last_modifier = modifier;
        }
        let next_fg = span.style.fg.unwrap_or(Color::Reset);
        let next_bg = span.style.bg.unwrap_or(Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetColors(Colors::new(
                    next_fg.into_crossterm(),
                    next_bg.into_crossterm()
                ))
            )?;
            fg = next_fg;
            bg = next_bg;
        }

        queue!(writer, Print(span.content.clone()))?;
    }

    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )
}
