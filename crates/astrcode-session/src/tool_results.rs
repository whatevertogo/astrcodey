//! Tool result budgeting and LLM-facing persisted-result summaries.

use astrcode_core::tool::{ToolResult, read_image::ReadToolInlinePayload};
use astrcode_storage::ToolResultArtifactRef;

/// 单个工具结果允许直接进入 LLM history 的最大字节数。
///
/// 该限制属于 Session 的统一上下文预算，而非某个工具的参数。扩展无需各自暴露
/// `maxOutputTokens` 一类调优字段；所有工具在同一提交边界获得相同的落盘与预览语义。
pub(crate) const TOOL_RESULT_INLINE_LIMIT_BYTES: usize = 30_000;

/// 摘要中保留的预览字符数，包含中间省略标记。
pub(crate) const TOOL_RESULT_PREVIEW_CHARS: usize = 2_000;
const TOOL_RESULT_OMISSION: &str = "\n[... middle omitted ...]\n";

/// 工具结果摘要预览。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultPreview {
    /// 摘要中内联展示的头尾内容。
    pub content: String,
    /// 原始内容是否还有更多未展示部分。
    pub has_more: bool,
}

/// 是否可以把结果自动替换为持久化 artifact 摘要。
pub(crate) fn should_auto_persist_tool_result(result: &ToolResult) -> bool {
    result.content.len() > TOOL_RESULT_INLINE_LIMIT_BYTES
        && (result.is_error || ReadToolInlinePayload::parse_image(&result.content).is_none())
}

/// 为大工具结果生成摘要预览。
pub(crate) fn tool_result_preview(content: &str, max_chars: usize) -> ToolResultPreview {
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    let has_more = chars.next().is_some();
    let marker_chars = TOOL_RESULT_OMISSION.chars().count();
    if !has_more || max_chars <= marker_chars {
        return ToolResultPreview {
            content: preview,
            has_more,
        };
    }

    let content_budget = max_chars - marker_chars;
    let head_chars = content_budget / 2;
    let tail_chars = content_budget.div_ceil(2);
    let head_end = preview
        .char_indices()
        .nth(head_chars)
        .map_or(preview.len(), |(i, _)| i);
    let tail_start = content
        .char_indices()
        .rev()
        .nth(tail_chars - 1)
        .map_or(content.len(), |(i, _)| i);
    ToolResultPreview {
        content: format!(
            "{}{}{}",
            &preview[..head_end],
            TOOL_RESULT_OMISSION,
            &content[tail_start..]
        ),
        has_more,
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
    fn auto_persist_eligibility_ignores_tool_identity_and_metadata_but_preserves_images() {
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

        let image = ReadToolInlinePayload::image("image/png", large_content)
            .to_content_string()
            .unwrap();
        assert!(!should_auto_persist_tool_result(&ToolResult::success(
            image
        )));
    }

    #[test]
    fn preview_preserves_head_and_tail_within_budget() {
        for unit in ["x", "中", "🦀"] {
            let content = format!("BEGIN\n{}\nFAILED: final diagnostic", unit.repeat(30_001));
            let preview = tool_result_preview(&content, TOOL_RESULT_PREVIEW_CHARS);
            assert!(preview.content.starts_with("BEGIN\n"));
            assert!(preview.content.ends_with("FAILED: final diagnostic"));
            assert!(preview.content.contains(TOOL_RESULT_OMISSION));
            assert_eq!(preview.content.chars().count(), TOOL_RESULT_PREVIEW_CHARS);
            assert!(preview.has_more);
        }
        let content = "中文🦀".repeat(30);
        for budget in 0..=content.chars().count() + 1 {
            let preview = tool_result_preview(&content, budget);
            assert!(preview.content.chars().count() <= budget);
            assert_eq!(preview.has_more, content.chars().count() > budget);
            if !preview.has_more {
                assert_eq!(preview.content, content);
            }
        }
        assert_eq!(
            tool_result_preview("", 0),
            ToolResultPreview {
                content: String::new(),
                has_more: false
            }
        );
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
