//! Tool result budgeting and LLM-facing persisted-result summaries.

use astrcode_core::tool::ToolResult;
use astrcode_storage::ToolResultArtifactRef;

/// 默认允许内联到 LLM history 的工具结果字节数。
pub const DEFAULT_TOOL_RESULT_INLINE_LIMIT: usize = 50_000;

/// shell 类工具输出更容易爆量，采用更低的默认阈值。
pub const SHELL_TOOL_RESULT_INLINE_LIMIT: usize = 30_000;

/// 搜索工具结果通常可重新分页查询，采用更低的默认阈值。
pub const GREP_TOOL_RESULT_INLINE_LIMIT: usize = 20_000;

/// read 工具输出由 maxChars 自行截断；再持久化到 tool-results 后让模型用 read
/// 读回会形成循环（Claude Code 对 Read 使用 Infinity 阈值同理），故永不自动持久化。
pub const READ_TOOL_RESULT_INLINE_LIMIT: Option<usize> = None;

/// 同一轮工具结果进入 LLM history 的总预算。
pub const MAX_TOOL_RESULTS_PER_MESSAGE_CHARS: usize = 200_000;

/// 摘要中保留的预览字符数（与 Claude Code PREVIEW_SIZE_BYTES ≈ 2000 对齐）。
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
        "read" => READ_TOOL_RESULT_INLINE_LIMIT,
        "shell" => Some(SHELL_TOOL_RESULT_INLINE_LIMIT),
        "grep" => Some(GREP_TOOL_RESULT_INLINE_LIMIT),
        _ => Some(DEFAULT_TOOL_RESULT_INLINE_LIMIT),
    }
}

/// 是否可以把结果自动替换为持久化 artifact 摘要。
///
/// read 自身的结果不自动持久化；是否已持久化由 session 私有提交状态判断。
pub fn should_auto_persist_tool_result(tool_name: &str, result: &ToolResult) -> bool {
    let Some(inline_limit) = tool_result_inline_limit(tool_name) else {
        return false;
    };
    should_persist_tool_result(&result.content, inline_limit)
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
             {path}\nUse read with path {:?}, charOffset, and maxChars to paginate through the \
             saved file. Do not expect the full content inline — increase charOffset on each read \
             until hasMore is false.\n\nPreview:\n{}{}",
            reference.bytes, path, preview.content, more
        ),
        None => format!(
            "Tool result was persisted because it is large ({} bytes), but this storage backend \
             did not expose a readable path.\n\nPreview:\n{}{}",
            reference.bytes, preview.content, more
        ),
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(
            tool_result_inline_limit("read"),
            READ_TOOL_RESULT_INLINE_LIMIT
        );
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
    fn auto_persist_eligibility_ignores_legacy_metadata_control_keys() {
        let large_content = "a".repeat(DEFAULT_TOOL_RESULT_INLINE_LIMIT + 1);
        let cases = [
            ("eligible", "example", Default::default(), true),
            ("read result", "read", Default::default(), false),
            (
                "legacy persisted key",
                "example",
                std::collections::BTreeMap::from([(
                    ["persistedTool", "Result"].concat(),
                    serde_json::json!(true),
                )]),
                true,
            ),
            (
                "legacy artifact source",
                "example",
                std::collections::BTreeMap::from([(
                    "source".into(),
                    serde_json::json!("toolResultArtifact"),
                )]),
                true,
            ),
        ];

        for (name, tool_name, metadata, expected) in cases {
            let result = ToolResult {
                content: large_content.clone(),
                is_error: false,
                error: None,
                metadata,
                duration_ms: None,
            };
            assert_eq!(
                should_auto_persist_tool_result(tool_name, &result),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn preview_reports_more_content() {
        let preview = tool_result_preview("abcdef", 3);

        assert_eq!(preview.content, "abc");
        assert!(preview.has_more);
    }

    #[test]
    fn summary_names_read_file_path() {
        let path = "/sessions/session-1/tool-results/shell-call-1.txt";
        let reference = ToolResultArtifactRef {
            bytes: 2048,
            path: Some(path.to_string()),
        };
        let preview = ToolResultPreview {
            content: "first lines".into(),
            has_more: true,
        };

        let summary = persisted_tool_result_summary(&reference, &preview);

        assert!(summary.contains("read"));
        assert!(summary.contains(path));
        assert!(summary.contains("Preview"));
        assert!(summary.contains("first lines"));
        assert!(summary.contains("More output"));
    }
}
