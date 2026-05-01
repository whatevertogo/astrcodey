//! 底部 composer 的输入状态与编辑行为。
//!
//! 光标位置使用 char 索引，便于和终端宽字符布局保持一致。

use unicode_width::UnicodeWidthChar;

const PASTE_PLACEHOLDER_THRESHOLD: usize = 1200;

/// Composer 可执行的编辑动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    InsertChar(char),
    InsertPaste(String),
    Newline,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    MoveVisualUp { width: usize },
    MoveVisualDown { width: usize },
    DeleteBeforeCursor,
    DeleteAfterCursor,
    DeletePreviousWord,
}

/// 输入框内部状态。
#[derive(Debug, Clone, Default)]
pub struct ComposerState {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    pasted: Vec<PastedContent>,
}

#[derive(Debug, Clone)]
struct PastedContent {
    placeholder: String,
    text: String,
}

impl ComposerState {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_text(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.text = text;
        self.history_idx = None;
        self.pasted.clear();
    }

    pub fn apply(&mut self, action: ComposerAction) -> bool {
        match action {
            ComposerAction::InsertChar(ch) => self.insert_char(ch),
            ComposerAction::InsertPaste(text) => self.insert_paste(&text),
            ComposerAction::Newline => self.insert_char('\n'),
            ComposerAction::Backspace => self.backspace(),
            ComposerAction::Delete => self.delete(),
            ComposerAction::MoveLeft => self.move_left(),
            ComposerAction::MoveRight => self.move_right(),
            ComposerAction::MoveHome => self.move_home(),
            ComposerAction::MoveEnd => self.move_end(),
            ComposerAction::MoveVisualUp { width } => self.move_visual_up(width),
            ComposerAction::MoveVisualDown { width } => self.move_visual_down(width),
            ComposerAction::DeleteBeforeCursor => self.delete_before_cursor(),
            ComposerAction::DeleteAfterCursor => self.delete_after_cursor(),
            ComposerAction::DeletePreviousWord => self.delete_previous_word(),
        }
    }

    pub fn insert_char(&mut self, ch: char) -> bool {
        let byte_idx = self.cursor_byte_index();
        self.text.insert(byte_idx, ch);
        self.cursor += 1;
        self.history_idx = None;
        true
    }

    pub fn insert_str(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let byte_idx = self.cursor_byte_index();
        self.text.insert_str(byte_idx, text);
        self.cursor += text.chars().count();
        self.history_idx = None;
        true
    }

    pub fn insert_paste(&mut self, text: &str) -> bool {
        if text.chars().count() <= PASTE_PLACEHOLDER_THRESHOLD {
            return self.insert_str(text);
        }

        let char_count = text.chars().count();
        let placeholder = format!("[Pasted Content {char_count} chars]");
        self.pasted.push(PastedContent {
            placeholder: placeholder.clone(),
            text: text.to_string(),
        });
        self.insert_str(&placeholder)
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.cursor - 1;
        self.replace_char_range(start, self.cursor, "");
        self.cursor = start;
        self.history_idx = None;
        true
    }

    pub fn delete(&mut self) -> bool {
        let char_count = self.char_count();
        if self.cursor >= char_count {
            return false;
        }
        self.replace_char_range(self.cursor, self.cursor + 1, "");
        self.history_idx = None;
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn move_right(&mut self) -> bool {
        let char_count = self.char_count();
        if self.cursor >= char_count {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn move_home(&mut self) -> bool {
        let new_cursor = self.current_line_start();
        if new_cursor == self.cursor {
            return false;
        }
        self.cursor = new_cursor;
        true
    }

    pub fn move_end(&mut self) -> bool {
        let new_cursor = self.current_line_end();
        if new_cursor == self.cursor {
            return false;
        }
        self.cursor = new_cursor;
        true
    }

    pub fn move_visual_up(&mut self, width: usize) -> bool {
        self.move_visual(width, -1)
    }

    pub fn move_visual_down(&mut self, width: usize) -> bool {
        self.move_visual(width, 1)
    }

    pub fn delete_before_cursor(&mut self) -> bool {
        let start = self.current_line_start();
        if self.cursor == start {
            return false;
        }
        self.replace_char_range(start, self.cursor, "");
        self.cursor = start;
        self.history_idx = None;
        true
    }

    pub fn delete_after_cursor(&mut self) -> bool {
        let end = self.current_line_end();
        if self.cursor >= end {
            return false;
        }
        self.replace_char_range(self.cursor, end, "");
        self.history_idx = None;
        true
    }

    pub fn delete_previous_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.cursor {
            return false;
        }

        self.replace_char_range(start, self.cursor, "");
        self.cursor = start;
        self.history_idx = None;
        true
    }

    pub fn take_submit_text(&mut self) -> String {
        let expanded = self.expanded_text();
        self.text.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.pasted.clear();
        expanded
    }

    pub fn remember_input(&mut self, input: &str) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(|value| value.as_str()) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
    }

    pub fn history_previous(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_idx = match self.history_idx {
            Some(idx) if idx > 0 => idx - 1,
            Some(idx) => idx,
            None => self.history.len().saturating_sub(1),
        };
        self.history_idx = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor = self.char_count();
        self.pasted.clear();
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.text.clear();
            self.cursor = 0;
            self.pasted.clear();
            return true;
        }
        let next_idx = idx + 1;
        self.history_idx = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor = self.char_count();
        self.pasted.clear();
        true
    }

    fn expanded_text(&self) -> String {
        let mut expanded = self.text.clone();
        for paste in &self.pasted {
            expanded = expanded.replacen(&paste.placeholder, &paste.text, 1);
        }
        expanded
    }

    fn move_visual(&mut self, width: usize, direction: i8) -> bool {
        let layout = VisualLayout::new(&self.text, width.max(1));
        let Some((row, column)) = layout.cursor_position(self.cursor) else {
            return false;
        };
        let target_row = match direction {
            -1 if row > 0 => row - 1,
            1 if row + 1 < layout.lines.len() => row + 1,
            _ => return false,
        };
        self.cursor = layout.index_near_column(target_row, column);
        true
    }

    fn current_line_start(&self) -> usize {
        let mut start = 0;
        for (idx, ch) in self.text.chars().enumerate().take(self.cursor) {
            if ch == '\n' {
                start = idx + 1;
            }
        }
        start
    }

    fn current_line_end(&self) -> usize {
        self.text
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find_map(|(idx, ch)| (ch == '\n').then_some(idx))
            .unwrap_or_else(|| self.char_count())
    }

    fn cursor_byte_index(&self) -> usize {
        self.byte_index(self.cursor)
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.text.len())
    }

    fn replace_char_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(end);
        self.text.replace_range(start_byte..end_byte, replacement);
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }
}

#[derive(Debug)]
struct VisualLayout {
    lines: Vec<VisualLine>,
    widths: Vec<usize>,
}

#[derive(Debug)]
struct VisualLine {
    start: usize,
    end: usize,
}

impl VisualLayout {
    fn new(text: &str, width: usize) -> Self {
        let mut lines = Vec::new();
        let widths = text.chars().map(char_width).collect::<Vec<_>>();
        let mut current_width = 0usize;
        let mut line_start = 0usize;
        let mut consumed_chars = 0usize;

        for ch in text.chars() {
            if ch == '\n' {
                lines.push(VisualLine {
                    start: line_start,
                    end: consumed_chars,
                });
                consumed_chars += 1;
                line_start = consumed_chars;
                current_width = 0;
                continue;
            }

            let ch_width = char_width(ch);
            if current_width + ch_width > width && line_start < consumed_chars {
                lines.push(VisualLine {
                    start: line_start,
                    end: consumed_chars,
                });
                line_start = consumed_chars;
                current_width = 0;
            }

            current_width += ch_width;
            consumed_chars += 1;
        }

        lines.push(VisualLine {
            start: line_start,
            end: consumed_chars,
        });

        Self { lines, widths }
    }

    fn cursor_position(&self, cursor: usize) -> Option<(usize, usize)> {
        for (row, line) in self.lines.iter().enumerate() {
            if cursor >= line.start && cursor <= line.end {
                return Some((row, self.column_at(row, cursor)));
            }
        }
        None
    }

    fn column_at(&self, row: usize, cursor: usize) -> usize {
        let Some(line) = self.lines.get(row) else {
            return 0;
        };
        let end = cursor.min(line.end);
        (line.start..end)
            .map(|idx| self.widths.get(idx).copied().unwrap_or(1))
            .sum()
    }

    fn index_near_column(&self, row: usize, column: usize) -> usize {
        let Some(line) = self.lines.get(row) else {
            return 0;
        };
        if column == 0 {
            return line.start;
        }
        let mut current = 0usize;
        for idx in line.start..line.end {
            let width = self.widths.get(idx).copied().unwrap_or(1);
            if current + width > column {
                return idx;
            }
            current += width;
        }
        line.end
    }
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_inside_wrapped_visual_lines_before_history() {
        let mut composer = ComposerState::default();
        composer.set_text("abcdef".into());
        composer.move_end();

        assert!(composer.move_visual_up(3));
        assert_eq!(composer.cursor(), 3);
        assert!(!composer.move_visual_up(3));

        assert!(composer.move_visual_down(3));
        assert_eq!(composer.cursor(), 6);
        assert!(!composer.move_visual_down(3));
    }

    #[test]
    fn keeps_wide_char_columns_stable() {
        let mut composer = ComposerState::default();
        composer.set_text("你a\n好b".into());
        composer.move_end();

        assert!(composer.move_visual_up(8));
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn home_end_and_ctrl_deletes_edit_expected_ranges() {
        let mut composer = ComposerState::default();
        composer.set_text("hello world\nsecond line".into());
        composer.move_end();
        assert!(composer.move_home());
        assert_eq!(composer.cursor(), 12);
        assert!(composer.move_end());
        assert_eq!(composer.cursor(), composer.text().chars().count());

        assert!(composer.delete_previous_word());
        assert_eq!(composer.text(), "hello world\nsecond ");
        assert!(composer.delete_before_cursor());
        assert_eq!(composer.text(), "hello world\n");

        composer.set_text("abc def".into());
        composer.move_home();
        assert!(composer.delete_after_cursor());
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn long_paste_uses_placeholder_but_submits_original_text() {
        let mut composer = ComposerState::default();
        let text = "x".repeat(PASTE_PLACEHOLDER_THRESHOLD + 1);

        assert!(composer.insert_paste(&text));
        assert_eq!(
            composer.text(),
            format!("[Pasted Content {} chars]", PASTE_PLACEHOLDER_THRESHOLD + 1)
        );
        assert_eq!(composer.take_submit_text(), text);
        assert!(composer.text().is_empty());
    }
}
