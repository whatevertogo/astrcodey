use astrcode_core::tool::{ToolDefinition, ToolOrigin, ToolPromptMetadata, ToolPromptTag};

use super::SystemPromptInput;

const TOOL_GUIDANCE: &str = "Read before you write; search before you ask.\nMatching workflow → \
                             `Skill` | External MCP only → `tool_search_tool` (not for builtin \
                             tools like `glob`) | Substantial independent subtask → `agent`";
const TOOL_SECTION_BUILTIN: &str = "Builtin Tools";
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
    let mut builtin = Vec::new();
    let mut mcp_tools = Vec::new();
    let mut extension_tools = Vec::new();
    for tool in input.tools {
        if is_collab(&tool) {
            collab.push(tool);
        } else if tool.name.starts_with("mcp__") {
            mcp_tools.push(tool);
        } else {
            match tool.origin {
                ToolOrigin::Builtin => builtin.push(tool),
                ToolOrigin::Bundled | ToolOrigin::Extension | ToolOrigin::Sdk => {
                    extension_tools.push(tool);
                },
            }
        }
    }
    builtin.sort_by_key(|tool| (tool_summary_rank(&tool.name), tool.name.clone()));

    let mut lines = Vec::new();
    if !builtin.is_empty() {
        lines.push(TOOL_SECTION_BUILTIN.into());
        push_tool_list_entries(&mut lines, &builtin, true);
    }
    if !collab.is_empty() {
        push_tool_section(
            &mut lines,
            TOOL_SECTION_AGENT_COLLABORATION,
            Some(TOOL_AGENT_COLLABORATION_GUIDANCE),
        );
        push_tool_list_entries(&mut lines, &collab, false);
    }

    if !mcp_tools.is_empty() {
        push_tool_section(&mut lines, TOOL_SECTION_EXTERNAL_MCP, None);
        push_tool_list_entries(&mut lines, &mcp_tools, false);
    }

    if !extension_tools.is_empty() {
        push_tool_section(&mut lines, TOOL_SECTION_EXTENSION, None);
        push_tool_list_entries(&mut lines, &extension_tools, false);
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

fn push_tool_list_entries(
    lines: &mut Vec<String>,
    tools: &[&ToolDefinition],
    with_short_desc: bool,
) {
    for tool in tools {
        if with_short_desc {
            let short = tool_short_description(&tool.name);
            if short.is_empty() {
                lines.push(format!("- `{}`", tool.name));
            } else {
                lines.push(format!("- `{}`: {}", tool.name, short));
            }
        } else {
            lines.push(format!("- `{}`", tool.name));
        }
    }
}

fn tool_summary_rank(name: &str) -> u8 {
    match name {
        "read" => 0,
        "glob" => 1,
        "grep" => 2,
        "shell" => 3,
        "tool_search_tool" => 4,
        "Skill" => 5,
        "todoWrite" => 6,
        "switchMode" => 7,
        "upsertSessionPlan" => 8,
        "agent" => 9,
        "patch" => 90,
        "edit" => 91,
        "write" => 92,
        _ => 50,
    }
}

fn tool_short_description(name: &str) -> &'static str {
    match name {
        "read" => "read file content with line numbers",
        "glob" => "match file paths by glob pattern",
        "grep" => "search file contents by regex or literal text",
        "shell" => "execute shell commands",
        "terminal" => "manage interactive PTY sessions",
        "tool_search_tool" => "find MCP tools by name or keyword",
        "Skill" => "load a named skill's instructions",
        "todoWrite" => "update session progress todo list",
        "switchMode" => "switch between code and plan modes",
        "upsertSessionPlan" => "create or update the session plan",
        "agent" => "delegate to a specialized [Agents] subagent",
        "patch" => "apply unified diff across multiple files",
        "edit" => "exact string replacement in a file",
        "write" => "create or completely overwrite a file",
        _ => "",
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
