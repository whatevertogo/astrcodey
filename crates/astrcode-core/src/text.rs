//! 跨边界文本摘要原语。

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

#[cfg(test)]
mod tests {
    use super::*;

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
