//! Markdown frontmatter splitting for extension-defined documents.

/// Normalize line endings (CRLF/CR → LF) and strip the BOM, for use before frontmatter splitting.
pub fn normalize_markdown(content: &str) -> String {
    content
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Splits Markdown that starts with YAML frontmatter into its YAML and body.
///
/// The closing delimiter must be a line containing only `---` or `...`.
pub fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let mut offset = 0;

    for line in rest.split_inclusive('\n') {
        let delimiter = line.strip_suffix('\n').unwrap_or(line);
        if matches!(delimiter, "---" | "...") {
            let frontmatter = rest[..offset].strip_suffix('\n').unwrap_or(&rest[..offset]);
            return Some((frontmatter, &rest[offset + line.len()..]));
        }
        offset += line.len();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::split_frontmatter;

    #[test]
    fn splits_only_well_formed_frontmatter() {
        for (content, expected) in [
            (
                "---\nname: test\n---\nbody text",
                Some(("name: test", "body text")),
            ),
            ("---\nname: test\n...\nbody", Some(("name: test", "body"))),
            ("---\nname: test\n---", Some(("name: test", ""))),
            ("---\n---\nbody", Some(("", "body"))),
            ("no frontmatter", None),
            ("---\nname: test\nno close", None),
            ("---\nvalue: before --- after", None),
        ] {
            assert_eq!(split_frontmatter(content), expected);
        }
    }
}
