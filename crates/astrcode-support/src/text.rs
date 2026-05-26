//! 文本格式化工具：单行摘要生成。

/// 把任意文本压成单行摘要，超长时尾部追加 `…`。
///
/// 行为：
/// - 折叠所有空白序列为单个空格（与 `text.split_whitespace().join(" ")` 等价）。
/// - 按字符数（非字节数）截断到 `max_chars`；超出时附加 `…`（U+2026）。
/// - 长度计算基于 Unicode 标量值（`char`），对 ASCII 与 CJK 行为一致；
///   不做字形宽度感知，需要对齐显示宽度时调用方应另行处理。
///
/// 用于把工具调用参数、命令行、用户输入等折叠成可放进单行 UI 的预览。
pub fn compact_inline(text: &str, max_chars: usize) -> String {
    let mut compact = String::new();
    let mut words = text.split_whitespace();
    if let Some(first) = words.next() {
        compact.push_str(first);
        for word in words {
            compact.push(' ');
            compact.push_str(word);
        }
    }
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

/// 取首行，超长按字符数截断并追加 `…`。
///
/// 与 [`compact_inline`] 不同：保留首行的内部空白，丢弃后续行；不折叠空白。
/// 适合那些"已经是一行式但仍可能很长"的内容（错误信息、命令输出首行等）。
pub fn truncate_first_line(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.chars().count() <= max_chars {
        return first_line.to_string();
    }
    let mut s: String = first_line.chars().take(max_chars).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_first_line_keeps_short_input_unchanged() {
        assert_eq!(truncate_first_line("hello", 10), "hello");
    }

    #[test]
    fn truncate_first_line_drops_subsequent_lines() {
        assert_eq!(truncate_first_line("first\nsecond", 80), "first");
    }

    #[test]
    fn truncate_first_line_appends_ellipsis_when_over_limit() {
        let result = truncate_first_line("0123456789abcdef", 8);
        assert_eq!(result, "01234567…");
    }

    #[test]
    fn truncate_first_line_counts_characters_not_bytes() {
        // 4 个 CJK 字符 + 4 个 ASCII = 8 字符；max_chars=8 应保留全部
        let result = truncate_first_line("你好世界abcd", 8);
        assert_eq!(result, "你好世界abcd");
    }

    #[test]
    fn truncate_first_line_preserves_internal_whitespace() {
        assert_eq!(truncate_first_line("hello   world", 80), "hello   world");
    }

    #[test]
    fn compact_inline_empty_string() {
        assert_eq!(compact_inline("", 10), "");
    }

    #[test]
    fn compact_inline_collapses_whitespace() {
        assert_eq!(compact_inline("  hello   world  ", 80), "hello world");
    }

    #[test]
    fn compact_inline_truncates_at_char_boundary() {
        assert_eq!(compact_inline("0123456789", 5), "01234…");
    }

    #[test]
    fn compact_inline_exact_limit_no_ellipsis() {
        assert_eq!(compact_inline("abcde", 5), "abcde");
    }
}
