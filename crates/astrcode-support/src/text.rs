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
    let mut char_count = 0;
    let mut truncated = false;

    'words: for word in text.split_whitespace() {
        if char_count > 0 {
            if char_count == max_chars {
                truncated = true;
                break;
            }
            compact.push(' ');
            char_count += 1;
        }
        for ch in word.chars() {
            if char_count == max_chars {
                truncated = true;
                break 'words;
            }
            compact.push(ch);
            char_count += 1;
        }
    }

    if truncated {
        compact.push('…');
    }
    compact
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
    fn compact_inline_handles_whitespace_and_character_limits() {
        for (input, max_chars, expected) in [
            ("", 10, ""),
            ("  hello   world  ", 80, "hello world"),
            ("0123456789", 5, "01234…"),
            ("abcde", 5, "abcde"),
            ("content", 0, "…"),
            ("ab cd", 5, "ab cd"),
            ("ab cd", 2, "ab…"),
            ("ab cd", 3, "ab …"),
            ("你好 世界", 4, "你好 世…"),
        ] {
            assert_eq!(compact_inline(input, max_chars), expected);
        }
    }
}
