pub(crate) fn inline_preview(text: &str, max_chars: usize) -> String {
    let mut preview = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((byte_index, _)) = preview.char_indices().nth(max_chars) {
        preview.truncate(byte_index);
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::inline_preview;

    #[test]
    fn inline_preview_normalizes_and_truncates_display_text() {
        for (text, max_chars, expected) in [
            ("", 10, ""),
            ("  hello   world  ", 80, "hello world"),
            ("0123456789", 5, "01234…"),
            ("abcde", 5, "abcde"),
            ("content", 0, "…"),
            ("ab cd", 3, "ab …"),
            ("你好 世界", 4, "你好 世…"),
        ] {
            assert_eq!(inline_preview(text, max_chars), expected);
        }
    }
}
