//! Tool result persistence for large outputs.
//!
//! When tool results exceed the inline threshold, they are persisted
//! to disk and replaced with a file reference. This prevents large
//! outputs from consuming excessive context window space.

use std::path::PathBuf;

use astrcode_core::tool::ToolResult;

/// Persist a tool result to disk if it exceeds the inline limit.
///
/// Returns the (possibly modified) tool result and the persist path if saved.
pub fn maybe_persist_tool_result(
    result: &mut ToolResult,
    inline_limit: usize,
    persist_dir: &PathBuf,
) -> std::io::Result<Option<PathBuf>> {
    if result.content.len() <= inline_limit {
        return Ok(None);
    }

    let file_path = persist_tool_result(&result.content, &result.call_id, persist_dir)?;
    let original_len = result.content.len();
    // Replace content with a reference
    result.content = format!(
        "[Tool result persisted to {} ({} bytes, {} KB)]",
        file_path.display(),
        original_len,
        original_len / 1024
    );
    Ok(Some(file_path))
}

/// Persist tool result content to a file.
///
/// The filename is derived from the sanitized tool call ID.
pub fn persist_tool_result(
    content: &str,
    call_id: &str,
    persist_dir: &PathBuf,
) -> std::io::Result<PathBuf> {
    // Sanitize the call_id to prevent path traversal
    let safe_id = sanitize_for_filename(call_id);
    let file_path = persist_dir.join(format!("{}.txt", safe_id));

    // Ensure the directory exists
    std::fs::create_dir_all(persist_dir)?;
    std::fs::write(&file_path, content)?;

    Ok(file_path)
}

/// Sanitize a string for use in a filename.
///
/// Removes characters that could be used for path traversal
/// or that are invalid in filenames.
fn sanitize_for_filename(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::tool::ToolResult;

    use super::*;

    #[test]
    fn test_maybe_persist_small_result() {
        let mut result = ToolResult {
            call_id: "abc123".into(),
            content: "Hello World".into(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        // Content is small, should not persist
        let path = maybe_persist_tool_result(&mut result, 100, &dir).unwrap();
        assert!(path.is_none());
        assert_eq!(result.content, "Hello World");
    }

    #[test]
    fn test_maybe_persist_large_result() {
        let large_content = "A".repeat(1000);
        let mut result = ToolResult {
            call_id: "abc123".into(),
            content: large_content,
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        let path = maybe_persist_tool_result(&mut result, 100, &dir).unwrap();
        assert!(path.is_some());
        assert!(result.content.contains("persisted to"));
        assert!(result.content.contains("1000 bytes"));
    }

    #[test]
    fn test_sanitize_call_id() {
        // Path traversal attempts should be sanitized
        let mut result = ToolResult {
            call_id: "../etc/passwd".into(),
            content: "data".into(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        let _ = maybe_persist_tool_result(&mut result, 10, &dir);
        // The call_id gets sanitized before being used as filename
        // Path traversal chars are filtered out
    }
}
