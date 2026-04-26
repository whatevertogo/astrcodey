use std::{fmt, io, io::Write};

use crossterm::{
    Command,
    cursor::{MoveTo, MoveToColumn, RestorePosition, SavePosition},
    queue,
    style::{
        Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
        SetForegroundColor,
    },
    terminal::{Clear, ClearType},
};
use ratatui::{
    backend::Backend,
    layout::Size,
    style::{Color, Modifier},
    text::{Line, Span},
};

use super::custom_terminal::Terminal;
use crate::ui::{HistoryLine, materialize_history_line};

pub fn insert_history_lines<B>(
    terminal: &mut Terminal<B>,
    lines: Vec<HistoryLine>,
) -> io::Result<()>
where
    B: Backend<Error = io::Error> + Write,
{
    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));
    let mut area = terminal.viewport_area;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let mut should_update_area = false;
    let writer = terminal.backend_mut();

    let wrap_width = area.width.max(1) as usize;
    let mut wrapped = Vec::new();
    let mut wrapped_rows = 0usize;
    for line in &lines {
        let parts = wrap_line(line, wrap_width);
        wrapped_rows += parts
            .iter()
            .map(|wrapped_line| wrapped_line.width().max(1).div_ceil(wrap_width))
            .sum::<usize>();
        wrapped.extend(parts);
    }
    let wrapped_lines = wrapped_rows as u16;

    let cursor_top = if area.bottom() < screen_size.height {
        let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());
        let top_1based = area.top() + 1;
        queue!(writer, SetScrollRegion(top_1based..screen_size.height))?;
        queue!(writer, MoveTo(0, area.top()))?;
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
    queue!(writer, MoveTo(0, cursor_top))?;
    for line in &wrapped {
        queue!(writer, Print("\r\n"))?;
        write_history_line(writer, line, wrap_width)?;
    }
    queue!(writer, ResetScrollRegion)?;
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

fn wrap_line(line: &HistoryLine, width: usize) -> Vec<Line<'static>> {
    materialize_history_line(line, width)
}

fn write_history_line<W: Write>(
    writer: &mut W,
    line: &Line<'static>,
    wrap_width: usize,
) -> io::Result<()> {
    let physical_rows = line.width().max(1).div_ceil(wrap_width) as u16;
    if physical_rows > 1 {
        queue!(writer, SavePosition)?;
        for _ in 1..physical_rows {
            queue!(writer, crossterm::cursor::MoveDown(1), MoveToColumn(0))?;
            queue!(writer, Clear(ClearType::UntilNewLine))?;
        }
        queue!(writer, RestorePosition)?;
    }
    queue!(
        writer,
        SetColors(Colors::new(
            line.style
                .fg
                .map(to_crossterm_color)
                .unwrap_or(CColor::Reset),
            line.style
                .bg
                .map(to_crossterm_color)
                .unwrap_or(CColor::Reset)
        ))
    )?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    let merged_spans: Vec<Span<'_>> = line
        .spans
        .iter()
        .map(|span| Span {
            style: span.style.patch(line.style),
            content: span.content.clone(),
        })
        .collect();
    write_spans(writer, merged_spans.iter())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute SetScrollRegion using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute ResetScrollRegion using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
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

fn write_spans<'a, I>(writer: &mut impl Write, content: I) -> io::Result<()>
where
    I: IntoIterator<Item = &'a Span<'a>>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();

    for span in content {
        let mut next_modifier = Modifier::empty();
        next_modifier.insert(span.style.add_modifier);
        next_modifier.remove(span.style.sub_modifier);
        if next_modifier != modifier {
            let diff = ModifierDiff {
                from: modifier,
                to: next_modifier,
            };
            diff.queue(writer)?;
            modifier = next_modifier;
        }

        let next_fg = span.style.fg.unwrap_or(Color::Reset);
        let next_bg = span.style.bg.unwrap_or(Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetColors(Colors::new(
                    to_crossterm_color(next_fg),
                    to_crossterm_color(next_bg)
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

fn to_crossterm_color(color: Color) -> CColor {
    match color {
        Color::Reset => CColor::Reset,
        Color::Black => CColor::Black,
        Color::Red => CColor::DarkRed,
        Color::Green => CColor::DarkGreen,
        Color::Yellow => CColor::DarkYellow,
        Color::Blue => CColor::DarkBlue,
        Color::Magenta => CColor::DarkMagenta,
        Color::Cyan => CColor::DarkCyan,
        Color::Gray => CColor::Grey,
        Color::DarkGray => CColor::DarkGrey,
        Color::LightRed => CColor::Red,
        Color::LightGreen => CColor::Green,
        Color::LightYellow => CColor::Yellow,
        Color::LightBlue => CColor::Blue,
        Color::LightMagenta => CColor::Magenta,
        Color::LightCyan => CColor::Cyan,
        Color::White => CColor::White,
        Color::Rgb(r, g, b) => CColor::Rgb { r, g, b },
        Color::Indexed(index) => CColor::AnsiValue(index),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };

    use super::wrap_line;
    use crate::{state::WrappedLineRewrapPolicy, ui::HistoryLine};

    #[test]
    fn wrap_line_preserves_span_styles_after_rewrap() {
        let link_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::UNDERLINED);
        let line = Line::from(vec![
            Span::raw("> "),
            Span::styled("alpha beta", link_style),
        ]);

        let wrapped = wrap_line(
            &HistoryLine {
                line,
                rewrap_policy: WrappedLineRewrapPolicy::Reflow,
            },
            8,
        );

        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].to_string(), "> alpha");
        assert_eq!(wrapped[1].to_string(), "beta");
        assert!(wrapped[0].spans.iter().any(|span| span.style == link_style));
        assert!(wrapped[1].spans.iter().any(|span| span.style == link_style));
    }

    #[test]
    fn wrap_line_preserves_style_across_multiple_wrapped_rows() {
        let code_style = Style::default().bg(Color::DarkGray);
        let line = Line::from(vec![Span::styled("alpha beta gamma", code_style)]);

        let wrapped = wrap_line(
            &HistoryLine {
                line,
                rewrap_policy: WrappedLineRewrapPolicy::Reflow,
            },
            5,
        );

        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped[0].to_string(), "alpha");
        assert_eq!(wrapped[1].to_string(), "beta");
        assert_eq!(wrapped[2].to_string(), "gamma");
        assert!(
            wrapped
                .iter()
                .all(|line| line.spans.iter().all(|span| span.style == code_style))
        );
    }

    #[test]
    fn preserve_and_crop_keeps_single_row() {
        let line = Line::from("abcdefghijklmnopqrstuvwxyz");
        let wrapped = wrap_line(
            &HistoryLine {
                line,
                rewrap_policy: WrappedLineRewrapPolicy::PreserveAndCrop,
            },
            8,
        );

        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].to_string(), "abcdefg…");
    }
}
