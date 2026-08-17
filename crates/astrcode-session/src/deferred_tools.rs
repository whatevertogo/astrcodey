//! Deferred tool visibility for provider requests.

use std::{collections::HashSet, sync::Arc};

use astrcode_core::{
    llm::LlmMessage,
    tool::{ToolDefinition, ToolPromptMetadata},
};

#[derive(Clone)]
pub(crate) struct ToolSnapshot {
    pub definition: ToolDefinition,
    pub prompt_metadata: Option<ToolPromptMetadata>,
}

impl ToolSnapshot {
    pub(crate) fn definitions(tools: &[Self]) -> Vec<ToolDefinition> {
        tools.iter().map(|tool| tool.definition.clone()).collect()
    }
}

pub(crate) fn provider_visible_tools(
    tools: &[ToolSnapshot],
    active_deferred_tools: &HashSet<String>,
) -> Vec<ToolSnapshot> {
    tools
        .iter()
        .filter(|tool| {
            !is_deferred_tool(tool)
                || active_deferred_tools.contains(&tool.definition.name)
                || is_deferred_gate(tool)
        })
        .cloned()
        .collect()
}

pub(crate) fn append_deferred_tools_reminder(
    messages: &mut Vec<Arc<LlmMessage>>,
    tools: &[ToolSnapshot],
    active_deferred_tools: &HashSet<String>,
) {
    let deferred = tools
        .iter()
        .filter(|tool| is_deferred_tool(tool))
        .filter(|tool| !active_deferred_tools.contains(&tool.definition.name))
        .map(|tool| tool.definition.name.as_str())
        .collect::<Vec<_>>();
    if deferred.is_empty() || !tools.iter().any(is_deferred_gate) {
        return;
    }

    let mut text = String::from(
        "<available-deferred-tools>\nDeferred tools are listed by name only. Use the matching \
         discovery tool to fetch full schemas before calling one of these tools.\n",
    );
    for name in deferred {
        text.push_str(name);
        text.push('\n');
    }
    text.push_str("</available-deferred-tools>");
    messages.push(Arc::new(LlmMessage::system(text)));
}

pub(crate) fn activate_deferred_tools(
    active_deferred_tools: &mut HashSet<String>,
    tools: &[ToolSnapshot],
    discovered: Vec<String>,
) -> bool {
    let available = tools
        .iter()
        .filter(|tool| is_deferred_tool(tool))
        .map(|tool| tool.definition.name.as_str())
        .collect::<HashSet<_>>();
    let mut changed = false;
    for name in discovered {
        if available.contains(name.as_str()) {
            changed |= active_deferred_tools.insert(name);
        }
    }
    changed
}

pub(crate) fn tool_is_visible(tools: &[ToolDefinition], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

/// 常见误用工具名 → 本环境实际工具名。
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("find", "glob"),
    ("glob_file_search", "glob"),
    ("list_files", "glob"),
    ("read_file", "read"),
    ("readfile", "read"),
    ("write_file", "write"),
    ("writefile", "write"),
];

pub(crate) fn suggest_tool_alias(requested: &str) -> Option<&'static str> {
    TOOL_ALIASES
        .iter()
        .find(|(alias, _)| requested.eq_ignore_ascii_case(alias))
        .map(|(_, target)| *target)
}

fn visible_tool_names_hint(visible_tools: &[ToolDefinition]) -> String {
    if visible_tools.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = visible_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    if names.len() <= 12 {
        format!(" Available tools: {}.", names.join(", "))
    } else {
        format!(
            " Available tools include: {} ({} total).",
            names[..10].join(", "),
            names.len()
        )
    }
}

/// 工具不在当前 turn 可见列表中时的 LLM 可见指引。
pub(crate) fn unavailable_tool_guidance(
    requested: &str,
    visible_tools: &[ToolDefinition],
    registered_tools: &[ToolDefinition],
) -> String {
    let registered = registered_tools.iter().any(|tool| tool.name == requested);
    let visible = tool_is_visible(visible_tools, requested);

    if registered && !visible {
        return format!(
            "Tool `{requested}` is registered but not loaded for this turn yet. For external MCP \
             tools, call `tool_search_tool` with a keyword from the tool name, then call the \
             matching `mcp__...` tool using the returned schema."
        );
    }

    if let Some(alias) = suggest_tool_alias(requested) {
        return format!(
            "Tool `{requested}` does not exist in this session. Use `{alias}` instead and read \
             its schema from the provider tool list."
        );
    }

    let mut message = format!("Tool `{requested}` is not available in this session.");
    message.push_str(&visible_tool_names_hint(visible_tools));
    message.push_str(" Use exact tool names from the provider tool list.");
    message
}

fn is_deferred_tool(tool: &ToolSnapshot) -> bool {
    tool.prompt_metadata
        .as_ref()
        .and_then(|metadata| metadata.deferred_discovery_group.as_ref())
        .is_some()
}

fn is_deferred_gate(tool: &ToolSnapshot) -> bool {
    tool.prompt_metadata
        .as_ref()
        .and_then(|metadata| metadata.deferred_discovery_gate.as_ref())
        .is_some()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use astrcode_core::tool::{ToolDefinition, ToolOrigin, ToolPromptMetadata};

    use super::*;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            strict: false,
            origin: ToolOrigin::Bundled,
        }
    }

    fn plain_snapshot(name: &str) -> ToolSnapshot {
        ToolSnapshot {
            definition: def(name),
            prompt_metadata: None,
        }
    }

    fn deferred_snapshot(name: &str, group: &str) -> ToolSnapshot {
        ToolSnapshot {
            definition: def(name),
            prompt_metadata: Some(ToolPromptMetadata::default().deferred_discovery_group(group)),
        }
    }

    fn gate_snapshot(name: &str, gate: &str) -> ToolSnapshot {
        ToolSnapshot {
            definition: def(name),
            prompt_metadata: Some(ToolPromptMetadata::default().deferred_discovery_gate(gate)),
        }
    }

    #[test]
    fn provider_visible_tools_follow_visibility_rules() {
        let cases = [
            (
                vec![plain_snapshot("read"), plain_snapshot("write")],
                HashSet::new(),
                vec!["read", "write"],
            ),
            (
                vec![
                    plain_snapshot("read"),
                    deferred_snapshot("mcp_tool", "group-a"),
                ],
                HashSet::new(),
                vec!["read"],
            ),
            (
                vec![
                    plain_snapshot("read"),
                    deferred_snapshot("mcp_tool", "group-a"),
                ],
                HashSet::from(["mcp_tool".to_string()]),
                vec!["read", "mcp_tool"],
            ),
            (
                vec![
                    deferred_snapshot("mcp_tool", "group-a"),
                    gate_snapshot("discover", "group-a"),
                ],
                HashSet::new(),
                vec!["discover"],
            ),
        ];

        for (tools, active, expected) in cases {
            let visible = provider_visible_tools(&tools, &active);
            let names = visible
                .iter()
                .map(|tool| tool.definition.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, expected);
        }
    }

    #[test]
    fn activate_only_inserts_available_tools() {
        let tools = vec![deferred_snapshot("a", "g"), deferred_snapshot("b", "g")];
        let mut active = HashSet::new();
        let changed = activate_deferred_tools(&mut active, &tools, vec!["a".into(), "c".into()]);
        assert!(changed);
        assert!(active.contains("a"));
        assert!(!active.contains("c"));
    }

    #[test]
    fn activate_returns_false_when_no_new_tools() {
        let tools = vec![deferred_snapshot("a", "g")];
        let mut active = HashSet::new();
        active.insert("a".into());
        let changed = activate_deferred_tools(&mut active, &tools, vec!["a".into()]);
        assert!(!changed);
    }

    #[test]
    fn visibility_aliases_and_unavailable_guidance_cover_each_resolution_path() {
        let visible = vec![
            def("glob"),
            def("grep"),
            def("read"),
            def("tool_search_tool"),
        ];
        assert!(tool_is_visible(&visible, "read"));
        assert!(!tool_is_visible(&visible, "shell"));
        assert_eq!(suggest_tool_alias("find"), Some("glob"));
        assert_eq!(suggest_tool_alias("list_files"), Some("glob"));
        assert_eq!(suggest_tool_alias("read_file"), Some("read"));
        assert_eq!(suggest_tool_alias("shell"), None);

        let legacy = unavailable_tool_guidance("find", &visible, &visible);
        assert!(legacy.contains("glob"));
        assert!(!legacy.contains("not loaded for this turn"));

        let mut registered = visible.clone();
        registered.push(def("mcp__demo__search"));
        let deferred = unavailable_tool_guidance("mcp__demo__search", &visible, &registered);
        assert!(deferred.contains("tool_search_tool"));
        assert!(deferred.contains("not loaded for this turn"));

        let unknown = unavailable_tool_guidance("missing_tool", &visible, &visible);
        assert!(unknown.contains("Available tools"));
        assert!(unknown.contains("glob"));
    }
}
