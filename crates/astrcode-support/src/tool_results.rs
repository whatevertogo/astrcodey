//! 工具结果 artifact 辅助函数。
//!
//! 大体积工具结果由 server 在统一提交点写入 session artifact 目录，LLM
//! history 只保留可分页读取的短引用。

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use astrcode_core::storage::{
    ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactSlice,
};

/// 默认允许内联到 LLM history 的工具结果字节数。
pub const DEFAULT_TOOL_RESULT_INLINE_LIMIT: usize = 50_000;

/// shell 类工具输出更容易爆量，采用更低的默认阈值。
pub const SHELL_TOOL_RESULT_INLINE_LIMIT: usize = 30_000;

/// 搜索工具结果通常可重新分页查询，采用更低的默认阈值。
pub const GREP_TOOL_RESULT_INLINE_LIMIT: usize = 20_000;

/// 同一轮工具结果进入 LLM history 的总预算。
pub const MAX_TOOL_RESULTS_PER_MESSAGE_CHARS: usize = 200_000;

/// 摘要中保留的预览字符数。
pub const TOOL_RESULT_PREVIEW_CHARS: usize = 2_000;

/// 工具结果摘要预览。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultPreview {
    /// 摘要中内联展示的前缀内容。
    pub content: String,
    /// 原始内容是否还有更多未展示部分。
    pub has_more: bool,
}

/// 判断工具结果是否应该持久化为 artifact。
pub fn should_persist_tool_result(content: &str, inline_limit: usize) -> bool {
    content.len() > inline_limit
}

/// 返回指定工具的内联阈值；`None` 表示永不自动持久化。
pub fn tool_result_inline_limit(tool_name: &str) -> Option<usize> {
    match tool_name {
        "read" => None,
        "shell" => Some(SHELL_TOOL_RESULT_INLINE_LIMIT),
        "grep" => Some(GREP_TOOL_RESULT_INLINE_LIMIT),
        _ => Some(DEFAULT_TOOL_RESULT_INLINE_LIMIT),
    }
}

/// 为大工具结果生成摘要预览。
pub fn tool_result_preview(content: &str, max_chars: usize) -> ToolResultPreview {
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    ToolResultPreview {
        content: preview,
        has_more: chars.next().is_some(),
    }
}

/// 生成 artifact 文件名。
pub fn tool_result_file_name(tool_name: &str, call_id: &str) -> String {
    let safe_tool = sanitize_for_filename(tool_name);
    let safe_call = sanitize_for_filename(call_id);
    format!("{safe_tool}-{safe_call}.txt")
}

/// 写入工具结果 artifact 正文。
pub fn write_tool_result_file(
    dir: &Path,
    input: &ToolResultArtifactInput,
    session_id: &str,
) -> std::io::Result<ToolResultArtifactRef> {
    std::fs::create_dir_all(dir)?;
    for suffix in 0..1000 {
        let file_name = tool_result_file_name_with_suffix(&input.tool_name, &input.call_id, suffix);
        let path = dir.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(input.content.as_bytes())?;
                return Ok(tool_result_ref(
                    session_id,
                    &input.tool_name,
                    &input.call_id,
                    input.content.len(),
                    Some(path),
                ));
            },
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if fs::read(&path)? == input.content.as_bytes() {
                    return Ok(tool_result_ref(
                        session_id,
                        &input.tool_name,
                        &input.call_id,
                        input.content.len(),
                        Some(path),
                    ));
                }
            },
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "too many tool result artifact filename collisions",
    ))
}

/// 构造 artifact 引用。
pub fn tool_result_ref(
    _session_id: &str,
    _tool_name: &str,
    _call_id: &str,
    bytes: usize,
    path: Option<PathBuf>,
) -> ToolResultArtifactRef {
    ToolResultArtifactRef {
        bytes,
        path: path.map(|path| path.display().to_string()),
    }
}

/// 返回给 LLM 的短摘要。
pub fn persisted_tool_result_summary(
    reference: &ToolResultArtifactRef,
    preview: &ToolResultPreview,
) -> String {
    let more = if preview.has_more {
        "\n\nMore output is available in the saved file."
    } else {
        ""
    };
    match reference.path.as_deref() {
        Some(path) => format!(
            "Tool result was persisted because it is large ({} bytes).\nFull output saved to: \
             {path}\nUse read with path {:?}, charOffset 0, and maxChars as needed to read \
             it.\n\nPreview:\n{}{}",
            reference.bytes, path, preview.content, more
        ),
        None => format!(
            "Tool result was persisted because it is large ({} bytes), but this storage backend \
             did not expose a readable path.\n\nPreview:\n{}{}",
            reference.bytes, preview.content, more
        ),
    }
}

/// 从 artifact 正文中读取一段字符切片。
pub fn slice_tool_result(
    path: &str,
    content: &str,
    char_offset: usize,
    max_chars: usize,
) -> ToolResultArtifactSlice {
    let mut iter = content.chars().skip(char_offset);
    let text: String = iter.by_ref().take(max_chars).collect();
    let returned_chars = text.chars().count();
    let has_more = iter.next().is_some();
    ToolResultArtifactSlice {
        path: path.to_string(),
        bytes: content.len(),
        char_offset,
        returned_chars,
        next_char_offset: has_more.then_some(char_offset.saturating_add(returned_chars)),
        has_more,
        content: text,
    }
}

fn sanitize_for_filename(input: &str) -> String {
    let sanitized = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect::<String>();
    if sanitized.is_empty() {
        "result".to_string()
    } else {
        sanitized
    }
}

fn tool_result_file_name_with_suffix(tool_name: &str, call_id: &str, suffix: usize) -> String {
    let base = tool_result_file_name(tool_name, call_id);
    if suffix == 0 {
        return base;
    }
    let stem = base.trim_end_matches(".txt");
    format!("{stem}-{suffix}.txt")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn small_result_stays_inline() {
        assert!(!should_persist_tool_result("hello", 100));
    }

    #[test]
    fn large_result_crosses_inline_limit() {
        assert!(should_persist_tool_result(&"a".repeat(101), 100));
    }

    #[test]
    fn tool_inline_limits_match_high_volume_tools() {
        assert_eq!(tool_result_inline_limit("read"), None);
        assert_eq!(
            tool_result_inline_limit("shell"),
            Some(SHELL_TOOL_RESULT_INLINE_LIMIT)
        );
        assert_eq!(
            tool_result_inline_limit("grep"),
            Some(GREP_TOOL_RESULT_INLINE_LIMIT)
        );
        assert_eq!(
            tool_result_inline_limit("unknown"),
            Some(DEFAULT_TOOL_RESULT_INLINE_LIMIT)
        );
    }

    #[test]
    fn preview_reports_more_content() {
        let preview = tool_result_preview("abcdef", 3);

        assert_eq!(preview.content, "abc");
        assert!(preview.has_more);
    }

    #[test]
    fn file_name_filters_path_segments() {
        assert_eq!(
            tool_result_file_name("shell/../../bad", "../call"),
            "shellbad-call.txt"
        );
    }

    #[test]
    fn summary_names_read_file_path() {
        let path = PathBuf::from("D:/sessions/session-1/tool-results/shell-call-1.txt");
        let reference = tool_result_ref("session-1", "shell", "call-1", 2048, Some(path.clone()));
        let preview = ToolResultPreview {
            content: "first lines".into(),
            has_more: true,
        };

        let summary = persisted_tool_result_summary(&reference, &preview);

        assert!(summary.contains("read"));
        assert!(summary.contains("path"));
        assert!(summary.contains(path.to_string_lossy().as_ref()));
        assert!(summary.contains("Preview"));
        assert!(summary.contains("first lines"));
        assert!(summary.contains("More output"));
    }

    #[test]
    fn writing_same_result_reuses_file_and_collision_uses_suffix() {
        let dir = unique_test_dir("tool-results");
        let input = ToolResultArtifactInput {
            call_id: "call-1".into(),
            tool_name: "shell".into(),
            content: "abcdef".into(),
        };

        let first = write_tool_result_file(&dir, &input, "session-1").unwrap();
        let second = write_tool_result_file(&dir, &input, "session-1").unwrap();
        assert_eq!(first.path, second.path);

        let changed = ToolResultArtifactInput {
            content: "changed".into(),
            ..input
        };
        let third = write_tool_result_file(&dir, &changed, "session-1").unwrap();
        assert_ne!(first.path, third.path);

        let first_path = PathBuf::from(first.path.unwrap());
        let third_path = PathBuf::from(third.path.unwrap());
        assert_eq!(std::fs::read_to_string(first_path).unwrap(), "abcdef");
        assert_eq!(std::fs::read_to_string(third_path).unwrap(), "changed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slices_text_with_next_offset() {
        let slice = slice_tool_result("D:/sessions/session/tool-results/call.txt", "abcdef", 2, 3);

        assert_eq!(slice.content, "cde");
        assert_eq!(slice.next_char_offset, Some(5));
        assert!(slice.has_more);
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
    }
}
