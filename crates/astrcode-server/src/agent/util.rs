//! Agent 辅助函数。
//!
//! 包含 JSON 参数修复、MCP 工具管理、工具可见性检查等工具函数。

use std::collections::HashSet;

use astrcode_core::{
    tool::{ToolDefinition, ToolResult},
    llm::LlmMessage,
};

use super::r#loop::{MCP_TOOL_PREFIX, TOOL_SEARCH_METADATA_KEY, TOOL_SEARCH_TOOL_NAME};

/// 解析并尝试修复 JSON 参数。
///
/// 某些 LLM 提供者（如 glm-5.1）可能生成格式不正确的 JSON。
/// 此函数尝试修复常见问题，如：
/// - 末尾缺少闭合括号
/// - 末尾有多余的逗号
/// - 引号不匹配
pub(super) fn parse_and_repair_json(arguments: &str, tool_name: &str) -> serde_json::Value {
    // 首先尝试直接解析
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        return value;
    }

    // 记录原始错误信息
    tracing::warn!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(200).collect::<String>(),
        arguments_len = arguments.len(),
        "Failed to parse tool call arguments, attempting repair"
    );

    // 尝试修复策略 1：去除末尾的逗号
    let trimmed = arguments.trim();
    if let Some(repaired) = trimmed.strip_suffix(',') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(repaired) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by removing trailing comma"
            );
            return value;
        }
    }

    // 尝试修复策略 2：关闭截断的字符串并补全缺失的闭合括号
    let repaired = close_truncated_json(trimmed);
    if repaired != trimmed {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&repaired) {
            tracing::debug!(
                tool = %tool_name,
                "Successfully repaired JSON by closing truncated content"
            );
            return value;
        }
    }

    // 所有修复尝试都失败，返回空对象
    tracing::error!(
        tool = %tool_name,
        arguments_preview = %arguments.chars().take(500).collect::<String>(),
        "All JSON repair attempts failed, using empty object"
    );
    serde_json::json!({})
}

pub(super) fn initially_active_mcp_tools(_tools: &[ToolDefinition]) -> HashSet<String> {
    HashSet::new()
}

pub(super) fn provider_visible_tools(
    tools: &[ToolDefinition],
    active_mcp_tools: &HashSet<String>,
) -> Vec<ToolDefinition> {
    tools
        .iter()
        .filter(|tool| {
            !is_concrete_mcp_tool(&tool.name)
                || active_mcp_tools.contains(&tool.name)
                || tool.name == TOOL_SEARCH_TOOL_NAME
        })
        .cloned()
        .collect()
}

pub(super) fn append_deferred_mcp_tools_reminder(
    messages: &mut Vec<LlmMessage>,
    tools: &[ToolDefinition],
    active_mcp_tools: &HashSet<String>,
) {
    let deferred = tools
        .iter()
        .filter(|tool| is_concrete_mcp_tool(&tool.name))
        .filter(|tool| !active_mcp_tools.contains(&tool.name))
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    if deferred.is_empty() || !tool_is_visible(tools, TOOL_SEARCH_TOOL_NAME) {
        return;
    }

    let mut text = String::from(
        "<available-deferred-mcp-tools>\nDeferred MCP tools are listed by name only. Use \
         tool_search_tool to fetch full schemas before calling one of these tools.\n",
    );
    for name in deferred {
        text.push_str(name);
        text.push('\n');
    }
    text.push_str("</available-deferred-mcp-tools>");
    messages.push(LlmMessage::system(text));
}

pub(super) fn activate_discovered_mcp_tools(
    active_mcp_tools: &mut HashSet<String>,
    tools: &[ToolDefinition],
    discovered: Vec<String>,
) -> bool {
    let available = tools
        .iter()
        .filter(|tool| is_concrete_mcp_tool(&tool.name))
        .map(|tool| tool.name.as_str())
        .collect::<HashSet<_>>();
    let mut changed = false;
    for name in discovered {
        if available.contains(name.as_str()) {
            changed |= active_mcp_tools.insert(name);
        }
    }
    changed
}

pub(super) fn discovered_mcp_tool_names(result: &ToolResult) -> Vec<String> {
    result
        .metadata
        .get(TOOL_SEARCH_METADATA_KEY)
        .and_then(|value| value.get("matches"))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|match_value| match_value.get("name").and_then(|value| value.as_str()))
        .filter(|name| is_concrete_mcp_tool(name))
        .map(str::to_string)
        .collect()
}

pub(super) fn tool_is_visible(tools: &[ToolDefinition], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

pub(super) fn is_concrete_mcp_tool(name: &str) -> bool {
    name.starts_with(MCP_TOOL_PREFIX) && name != TOOL_SEARCH_TOOL_NAME
}

/// 关闭截断的 JSON：补上未闭合的字符串引号和缺失的括号。
///
/// 常见场景：LLM 流式响应被中断，导致工具调用参数 JSON 被截断，
/// 如 `{"todos": [{"status": "com` → `{"todos": [{"status": "com"}]}`。
fn close_truncated_json(s: &str) -> String {
    let mut result = s.to_string();

    // 用栈跟踪嵌套层级，确保按正确逆序关闭括号
    let mut in_string = false;
    let mut escape_next = false;
    let mut bracket_stack: Vec<char> = Vec::new();

    for ch in result.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            match ch {
                '{' | '[' => bracket_stack.push(ch),
                '}' if bracket_stack.last() == Some(&'{') => {
                    bracket_stack.pop();
                },
                ']' if bracket_stack.last() == Some(&'[') => {
                    bracket_stack.pop();
                },
                _ => {},
            }
        }
    }

    // 补上缺失的闭合引号
    if in_string {
        result.push('"');
    }

    // 按嵌套逆序关闭剩余未闭合的括号
    while let Some(opening) = bracket_stack.pop() {
        match opening {
            '{' => result.push('}'),
            '[' => result.push(']'),
            _ => {},
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_truncated_json_closes_open_string() {
        let result = close_truncated_json(r#"{"todos": [{"status": "com"#);
        assert_eq!(result, r#"{"todos": [{"status": "com"}]}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["todos"][0]["status"], "com");
    }

    #[test]
    fn close_truncated_json_handles_escaped_quotes() {
        let result = close_truncated_json(r#"{"text": "say \"hello"#);
        assert_eq!(result, r#"{"text": "say \"hello"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["text"], r#"say "hello"#);
    }

    #[test]
    fn close_truncated_json_adds_brackets_without_string() {
        let result = close_truncated_json(r#"{"key": {"nested": [1, 2"#);
        assert_eq!(result, r#"{"key": {"nested": [1, 2]}}"#);
        let _: serde_json::Value = serde_json::from_str(&result).unwrap();
    }

    #[test]
    fn close_truncated_json_no_change_for_valid_json() {
        let input = r#"{"todos": []}"#;
        assert_eq!(close_truncated_json(input), input);
    }

    #[test]
    fn parse_and_repair_json_handles_truncated_string() {
        let result =
            parse_and_repair_json(r#"{"todos": [{"status": "com"#, "testTool");
        assert_eq!(result["todos"][0]["status"], "com");
    }

    #[test]
    fn parse_and_repair_json_returns_empty_on_garbage() {
        let result = parse_and_repair_json("not json at all {{{", "testTool");
        assert_eq!(result, serde_json::json!({}));
    }
}
