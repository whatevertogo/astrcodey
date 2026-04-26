use std::borrow::Cow;

use textwrap::{Options, WordSeparator, wrap};
use unicode_segmentation::UnicodeSegmentation;

pub fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        let options = if looks_like_path_or_url(raw_line) {
            base_options(width).break_words(true)
        } else {
            base_options(width)
        };

        let wrapped = wrap(raw_line, options);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped.into_iter().map(normalize_line));
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn base_options(width: usize) -> Options<'static> {
    Options::new(width)
        .word_separator(WordSeparator::AsciiSpace)
        .break_words(false)
}

fn looks_like_path_or_url(line: &str) -> bool {
    line.contains("://")
        || line.contains('\\')
        || line.matches('/').count() >= 2
        || line
            .split_word_bounds()
            .any(|segment| segment.contains('.') && segment.len() > 12)
}

fn normalize_line(line: Cow<'_, str>) -> String {
    line.trim_end_matches([' ', '\t']).to_string()
}

#[cfg(test)]
mod tests {
    use super::wrap_plain_text;

    #[test]
    fn wraps_chinese_and_keeps_whitespace_only_line_as_single_row() {
        let wrapped = wrap_plain_text("第一行\n\n这一段非常长，需要正确折行。", 8);
        assert!(wrapped.len() >= 3);
        assert!(wrapped.iter().any(|line| line.is_empty()));
    }

    #[test]
    fn wraps_long_path_without_dropping_tail() {
        let wrapped = wrap_plain_text("C:/very/long/path/to/a/really-large-file-name.txt", 12);
        assert!(wrapped.len() >= 2);
        assert!(wrapped.last().is_some_and(|line| line.contains("txt")));
    }
}
