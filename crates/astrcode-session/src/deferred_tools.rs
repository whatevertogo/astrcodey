//! Deferred tool visibility for provider requests.

use std::collections::HashSet;

use astrcode_core::{
    llm::LlmMessage,
    tool::{DEFERRED_TOOLS_METADATA_KEY, ToolDefinition, ToolPromptMetadata, ToolResult},
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

pub fn provider_visible_tool_indexes(
    tools: &[ToolSnapshot],
    active_deferred_tools: &HashSet<String>,
) -> Vec<usize> {
    tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| {
            !is_deferred_tool(tool)
                || active_deferred_tools.contains(&tool.definition.name)
                || is_deferred_gate(tool)
        })
        .map(|(index, _)| index)
        .collect()
}

pub fn clone_tools_by_index(tools: &[ToolSnapshot], indexes: &[usize]) -> Vec<ToolSnapshot> {
    indexes
        .iter()
        .filter_map(|index| tools.get(*index))
        .cloned()
        .collect()
}

pub fn append_deferred_tools_reminder(
    messages: &mut Vec<LlmMessage>,
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
    messages.push(LlmMessage::system(text));
}

pub fn activate_deferred_tools(
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

pub fn discovered_deferred_tool_names(result: &ToolResult) -> Vec<String> {
    result
        .metadata
        .get(DEFERRED_TOOLS_METADATA_KEY)
        .and_then(|value| value.get("matches"))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|match_value| match_value.as_str())
        .map(str::to_string)
        .collect()
}

pub fn tool_is_visible(tools: &[ToolDefinition], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
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

    use astrcode_core::tool::{
        DEFERRED_TOOLS_METADATA_KEY, ToolDefinition, ToolOrigin, ToolPromptMetadata, ToolResult,
    };

    use super::*;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            origin: ToolOrigin::Builtin,
            execution_mode: Default::default(),
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
    fn visible_indexes_normal_tools_always_shown() {
        let tools = vec![plain_snapshot("read"), plain_snapshot("write")];
        let active = HashSet::new();
        let indexes = provider_visible_tool_indexes(&tools, &active);
        assert_eq!(indexes, vec![0, 1]);
    }

    #[test]
    fn visible_indexes_deferred_tools_hidden_by_default() {
        let tools = vec![
            plain_snapshot("read"),
            deferred_snapshot("mcp_tool", "group-a"),
        ];
        let active = HashSet::new();
        let indexes = provider_visible_tool_indexes(&tools, &active);
        assert_eq!(indexes, vec![0]);
    }

    #[test]
    fn visible_indexes_deferred_tools_shown_when_activated() {
        let tools = vec![
            plain_snapshot("read"),
            deferred_snapshot("mcp_tool", "group-a"),
        ];
        let mut active = HashSet::new();
        active.insert("mcp_tool".into());
        let indexes = provider_visible_tool_indexes(&tools, &active);
        assert_eq!(indexes, vec![0, 1]);
    }

    #[test]
    fn visible_indexes_gate_always_visible() {
        let tools = vec![
            deferred_snapshot("mcp_tool", "group-a"),
            gate_snapshot("discover", "group-a"),
        ];
        let active = HashSet::new();
        let indexes = provider_visible_tool_indexes(&tools, &active);
        assert_eq!(indexes, vec![1]);
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
    fn discovered_names_extracts_matches() {
        let result = ToolResult {
            call_id: "c1".into(),
            content: String::new(),
            is_error: false,
            error: None,
            metadata: vec![(
                DEFERRED_TOOLS_METADATA_KEY.into(),
                serde_json::json!({ "matches": ["tool_a", "tool_b"] }),
            )]
            .into_iter()
            .collect(),
            duration_ms: None,
        };
        let names = discovered_deferred_tool_names(&result);
        assert_eq!(names, vec!["tool_a", "tool_b"]);
    }

    #[test]
    fn discovered_names_empty_when_no_metadata() {
        let result = ToolResult {
            call_id: "c1".into(),
            content: String::new(),
            is_error: false,
            error: None,
            metadata: Default::default(),
            duration_ms: None,
        };
        assert!(discovered_deferred_tool_names(&result).is_empty());
    }

    #[test]
    fn tool_is_visible_found() {
        let tools = vec![def("read"), def("write")];
        assert!(tool_is_visible(&tools, "read"));
        assert!(!tool_is_visible(&tools, "shell"));
    }
}
