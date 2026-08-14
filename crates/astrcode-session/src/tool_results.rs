//! Tool result budgeting and LLM-facing persisted-result summaries.

use astrcode_core::tool::ToolResult;
use astrcode_storage::ToolResultArtifactRef;

/// 单个工具结果允许直接进入 LLM history 的最大字节数。
///
/// 该限制属于 Session 的统一上下文预算，而非某个工具的参数。扩展无需各自暴露
/// `maxOutputTokens` 一类调优字段；所有工具在同一提交边界获得相同的落盘与预览语义。
pub(crate) const TOOL_RESULT_INLINE_LIMIT_BYTES: usize = 30_000;

/// 摘要中保留的预览字符数（与 Claude Code PREVIEW_SIZE_BYTES ≈ 2000 对齐）。
pub(crate) const TOOL_RESULT_PREVIEW_CHARS: usize = 2_000;

/// 工具结果摘要预览。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultPreview {
    /// 摘要中内联展示的前缀内容。
    pub content: String,
    /// 原始内容是否还有更多未展示部分。
    pub has_more: bool,
}

/// 是否可以把结果自动替换为持久化 artifact 摘要。
pub(crate) fn should_auto_persist_tool_result(result: &ToolResult) -> bool {
    result.content.len() > TOOL_RESULT_INLINE_LIMIT_BYTES
}

/// 为大工具结果生成摘要预览。
pub(crate) fn tool_result_preview(content: &str, max_chars: usize) -> ToolResultPreview {
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    ToolResultPreview {
        content: preview,
        has_more: chars.next().is_some(),
    }
}

/// 返回给 LLM 的短摘要。
pub(crate) fn persisted_tool_result_summary(
    reference: &ToolResultArtifactRef,
    preview: &ToolResultPreview,
) -> String {
    let more = if preview.has_more {
        "\n\nMore output is available in the saved artifact."
    } else {
        ""
    };
    format!(
        "Tool result was persisted because it is large ({} bytes).\nArtifact ID: {}\nUse \
         read_tool_result with artifactId {:?}, byteOffset, and maxBytes to paginate through the \
         saved result. Do not expect the full content inline — increase byteOffset on each read \
         until hasMore is false.\n\nPreview:\n{}{}",
        reference.bytes, reference.artifact_id, reference.artifact_id, preview.content, more
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_inline_limit_is_tool_agnostic() {
        let below = ToolResult::success("x".repeat(TOOL_RESULT_INLINE_LIMIT_BYTES));
        let above = ToolResult::success("x".repeat(TOOL_RESULT_INLINE_LIMIT_BYTES + 1));
        let maximum_artifact_page = ToolResult::success(
            "x".repeat(astrcode_extension_sdk::host::HOST_TOOL_RESULT_MAX_BYTES),
        );
        assert!(!should_auto_persist_tool_result(&below));
        assert!(should_auto_persist_tool_result(&above));
        assert!(!should_auto_persist_tool_result(&maximum_artifact_page));
    }

    #[test]
    fn auto_persist_eligibility_ignores_tool_identity_and_metadata() {
        let large_content = "a".repeat(TOOL_RESULT_INLINE_LIMIT_BYTES + 1);
        let cases = [
            ("plain result", Default::default()),
            (
                "arbitrary metadata",
                std::collections::BTreeMap::from([("custom".into(), serde_json::json!(true))]),
            ),
        ];

        for (name, metadata) in cases {
            let result = ToolResult {
                content: large_content.clone(),
                is_error: false,
                error: None,
                metadata,
                duration_ms: None,
            };
            assert!(should_auto_persist_tool_result(&result), "{name}");
        }
    }

    #[test]
    fn preview_reports_more_content() {
        let preview = tool_result_preview("abcdef", 3);

        assert_eq!(preview.content, "abc");
        assert!(preview.has_more);
    }

    #[test]
    fn summary_names_explicit_artifact_reader() {
        let artifact_id = "shell-call-1.txt";
        let reference = ToolResultArtifactRef {
            bytes: 2048,
            artifact_id: artifact_id.into(),
        };
        let preview = ToolResultPreview {
            content: "first lines".into(),
            has_more: true,
        };

        let summary = persisted_tool_result_summary(&reference, &preview);

        assert!(summary.contains("read_tool_result"));
        assert!(summary.contains(artifact_id));
        assert!(summary.contains("Preview"));
        assert!(summary.contains("first lines"));
        assert!(summary.contains("More output"));
    }
}
