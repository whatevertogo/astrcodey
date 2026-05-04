//! Markdown frontmatter splitting and YAML value parsing utilities.
//!
//! Shared by skill and agent discovery extensions that parse `.md` files
//! with YAML frontmatter.

/// Split Markdown content into YAML frontmatter and body.
///
/// Returns `Some((frontmatter, body))` when the content starts with `---\n`
/// and contains a closing `---` or `...` marker, or `None` otherwise.
pub fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    if !content.starts_with("---\n") {
        return None;
    }

    let rest = &content[4..];
    for marker in ["\n---\n", "\n...\n"] {
        if let Some(end) = rest.find(marker) {
            return Some((&rest[..end], &rest[end + marker.len()..]));
        }
    }
    for marker in ["\n---", "\n..."] {
        if let Some(end) = rest.find(marker) {
            if end + marker.len() == rest.len() {
                return Some((&rest[..end], ""));
            }
        }
    }
    None
}

/// Extract a trimmed string from a YAML value, handling String, Number, and Bool.
pub fn yaml_string_value(value: Option<&serde_yaml::Value>) -> Option<String> {
    match value? {
        serde_yaml::Value::String(text) => Some(text.trim().to_string()),
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        serde_yaml::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Parse a tools list from a YAML value, supporting CSV strings and sequences.
///
/// Accepts both `"read, grep"` (CSV string) and `["read", "grep"]` (YAML sequence).
pub fn yaml_parse_tools_list(value: Option<&serde_yaml::Value>) -> Vec<String> {
    match value {
        Some(serde_yaml::Value::String(text)) => split_csv(text),
        Some(serde_yaml::Value::Sequence(values)) => values
            .iter()
            .filter_map(|value| yaml_string_value(Some(value)))
            .flat_map(|value| split_csv(&value))
            .collect(),
        _ => Vec::new(),
    }
}

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_standard_frontmatter() {
        let content = "---\nname: test\n---\nbody text";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "name: test");
        assert_eq!(body, "body text");
    }

    #[test]
    fn splits_with_dots_marker() {
        let content = "---\nname: test\n...\nbody";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "name: test");
        assert_eq!(body, "body");
    }

    #[test]
    fn handles_trailing_marker_only() {
        let content = "---\nname: test\n---";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "name: test");
        assert_eq!(body, "");
    }

    #[test]
    fn returns_none_without_opening_marker() {
        assert!(split_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn returns_none_without_closing_marker() {
        assert!(split_frontmatter("---\nname: test\nno close").is_none());
    }

    #[test]
    fn extracts_string_value() {
        let v = serde_yaml::Value::String("  hello  ".into());
        assert_eq!(yaml_string_value(Some(&v)), Some("hello".to_string()));
    }

    #[test]
    fn extracts_number_value() {
        let v = serde_yaml::Value::Number(42.into());
        assert_eq!(yaml_string_value(Some(&v)), Some("42".to_string()));
    }

    #[test]
    fn extracts_bool_value() {
        let v = serde_yaml::Value::Bool(true);
        assert_eq!(yaml_string_value(Some(&v)), Some("true".to_string()));
    }

    #[test]
    fn returns_none_for_none() {
        assert_eq!(yaml_string_value(None), None);
    }

    #[test]
    fn parses_csv_tools() {
        let v = serde_yaml::Value::String("read, grep, shell".into());
        assert_eq!(
            yaml_parse_tools_list(Some(&v)),
            vec!["read", "grep", "shell"]
        );
    }

    #[test]
    fn parses_sequence_tools() {
        let v = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("read".into()),
            serde_yaml::Value::String("grep".into()),
        ]);
        assert_eq!(yaml_parse_tools_list(Some(&v)), vec!["read", "grep"]);
    }

    #[test]
    fn returns_empty_for_missing() {
        assert_eq!(yaml_parse_tools_list(None), Vec::<String>::new());
    }
}
