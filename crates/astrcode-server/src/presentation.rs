pub(crate) fn inline_preview(text: &str, max_chars: usize) -> String {
    let mut preview = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((byte_index, _)) = preview.char_indices().nth(max_chars) {
        preview.truncate(byte_index);
        preview.push('…');
    }
    preview
}
