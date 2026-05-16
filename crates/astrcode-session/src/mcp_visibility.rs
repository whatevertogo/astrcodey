//! MCP 工具可见性与发现管理。
//!
//! 控制 LLM 可见的 MCP 工具集合，管理 tool_search_tool 发现流程。

use std::collections::HashSet;

use astrcode_core::{
    llm::LlmMessage,
    tool::{ToolDefinition, ToolResult},
};

pub(crate) const MCP_TOOL_PREFIX: &str = "mcp__";
pub(crate) const TOOL_SEARCH_TOOL_NAME: &str = "tool_search_tool";
pub(crate) const TOOL_SEARCH_METADATA_KEY: &str = "toolSearch";

pub fn provider_visible_tool_indexes(
    tools: &[ToolDefinition],
    active_mcp_tools: &HashSet<String>,
) -> Vec<usize> {
    tools
        .iter()
        .enumerate()
        .filter(|tool| {
            let name = &tool.1.name;
            !is_concrete_mcp_tool(name)
                || active_mcp_tools.contains(name)
                || name == TOOL_SEARCH_TOOL_NAME
        })
        .map(|(index, _)| index)
        .collect()
}

pub fn clone_tools_by_index(tools: &[ToolDefinition], indexes: &[usize]) -> Vec<ToolDefinition> {
    indexes
        .iter()
        .filter_map(|index| tools.get(*index))
        .cloned()
        .collect()
}

pub fn append_deferred_mcp_tools_reminder(
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

pub fn activate_discovered_mcp_tools(
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

pub fn discovered_mcp_tool_names(result: &ToolResult) -> Vec<String> {
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

pub fn tool_is_visible(tools: &[ToolDefinition], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

pub fn is_concrete_mcp_tool(name: &str) -> bool {
    name.starts_with(MCP_TOOL_PREFIX) && name != TOOL_SEARCH_TOOL_NAME
}
