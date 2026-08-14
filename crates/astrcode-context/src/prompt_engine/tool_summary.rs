use astrcode_core::tool::{ToolDefinition, ToolPromptMetadata, ToolPromptTag};

use super::SystemPromptInput;

const TOOL_GUIDANCE: &str = "Read before you write; search before you ask.\nMatching workflow → \
                             `Skill` | External MCP only → `tool_search_tool` (not for local \
                             tools like `glob`) | Substantial independent subtask → `agent`";
const TOOL_SECTION_AGENT_COLLABORATION: &str = "Agent Collaboration Tools";
const TOOL_SECTION_EXTERNAL_MCP: &str = "External MCP Tools";
const TOOL_SECTION_EXTENSION: &str = "Extension Tools";
const TOOL_AGENT_COLLABORATION_GUIDANCE: &str =
    "- `subagentType` from [Agents]; see detailed guide for delegation patterns.";

pub(super) fn tool_summary(input: &SystemPromptInput<'_>) -> Option<String> {
    if input.tools.is_empty() {
        return None;
    }

    let is_collab = |tool: &&ToolDefinition| {
        input
            .tool_prompt_metadata
            .get(&tool.name)
            .map(|metadata| metadata.has_tag(ToolPromptTag::Collaboration))
            .unwrap_or(false)
    };
    let mut collab = Vec::new();
    let mut mcp_tools = Vec::new();
    let mut extension_tools = Vec::new();
    for tool in input.tools {
        if is_collab(&tool) {
            collab.push(tool);
        } else if tool.name.starts_with("mcp__") {
            mcp_tools.push(tool);
        } else {
            extension_tools.push(tool);
        }
    }

    let mut lines = Vec::new();
    if !collab.is_empty() {
        push_tool_section(
            &mut lines,
            TOOL_SECTION_AGENT_COLLABORATION,
            Some(TOOL_AGENT_COLLABORATION_GUIDANCE),
        );
        push_tool_list_entries(&mut lines, &collab);
    }

    if !mcp_tools.is_empty() {
        push_tool_section(&mut lines, TOOL_SECTION_EXTERNAL_MCP, None);
        push_tool_list_entries(&mut lines, &mcp_tools);
    }

    if !extension_tools.is_empty() {
        push_tool_section(&mut lines, TOOL_SECTION_EXTENSION, None);
        push_tool_list_entries(&mut lines, &extension_tools);
    }

    let detailed_guides: Vec<_> = input
        .tools
        .iter()
        .filter_map(|tool| {
            let metadata = input.tool_prompt_metadata.get(&tool.name)?;
            if metadata.should_render_detailed_guide() {
                build_detailed_guide(metadata)
            } else {
                None
            }
        })
        .collect();
    if !detailed_guides.is_empty() {
        lines.push(String::new());
        for guide in detailed_guides {
            lines.push(String::new());
            lines.push(guide);
        }
    }

    let body = if lines.is_empty() {
        TOOL_GUIDANCE.to_string()
    } else {
        format!("{TOOL_GUIDANCE}\n\n{}", lines.join("\n"))
    };
    Some(body.trim().to_string())
}

fn push_tool_section(lines: &mut Vec<String>, heading: &str, guidance: Option<&str>) {
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(heading.to_string());
    if let Some(guidance) = guidance {
        lines.push(guidance.to_string());
    }
}

fn push_tool_list_entries(lines: &mut Vec<String>, tools: &[&ToolDefinition]) {
    for tool in tools {
        lines.push(format!("- `{}`", tool.name));
    }
}

fn build_detailed_guide(metadata: &ToolPromptMetadata) -> Option<String> {
    let mut parts = vec![metadata.guide.clone()];
    if !metadata.caveats.is_empty() {
        parts.push(format!(
            "Caveats:\n{}",
            metadata
                .caveats
                .iter()
                .map(|caveat| format!("- {caveat}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !metadata.examples.is_empty() {
        parts.push(format!(
            "Examples:\n{}",
            metadata
                .examples
                .iter()
                .map(|example| format!("- {example}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    let body = parts.join("\n\n");
    (!body.trim().is_empty()).then_some(body)
}
