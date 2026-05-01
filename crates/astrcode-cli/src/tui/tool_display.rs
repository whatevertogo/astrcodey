//! Claude Code 风格的工具展示摘要。
//!
//! 这里只处理 CLI 内部显示：工具标签、参数摘要、结果摘要。
//! 协议事件和工具返回结构保持不变。

use astrcode_core::{
    render::{RenderKeyValue, RenderSpec, RenderTone},
    tool::ToolResult,
};

/// 单次工具展示的轻量描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDisplay {
    /// 工具行标签，例如 `Bash(cargo test)`。
    pub label: String,
    /// 工具正文摘要。运行中请求可能为空，完成后通常以 `⎿` 开头。
    pub body: String,
    /// 当前展示状态，用于上层决定消息角色和可见性。
    pub state: ToolDisplayState,
}

/// 工具展示状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDisplayState {
    /// 工具正在运行。
    Running,
    /// 工具成功完成。
    Completed,
    /// 工具失败。
    Error,
}

/// 判断工具是否应在消息记录中显示。
pub fn should_print_tool(tool_name: &str) -> bool {
    // 内部发现工具通常噪音较大；错误仍由完成事件单独显示。
    !matches!(tool_name, "tool_search")
}

/// 生成工具启动时的展示。
pub fn started(tool_name: &str) -> ToolDisplay {
    ToolDisplay {
        label: tool_label(tool_name, None),
        body: String::new(),
        state: ToolDisplayState::Running,
    }
}

/// 生成工具请求参数展示。
pub fn requested(tool_name: &str, arguments: &serde_json::Value) -> ToolDisplay {
    ToolDisplay {
        label: tool_label(tool_name, Some(arguments)),
        body: request_body(tool_name, arguments),
        state: ToolDisplayState::Running,
    }
}

/// 生成工具完成后的结果摘要。
pub fn completed(tool_name: &str, result: &ToolResult) -> ToolDisplay {
    let state = if result.is_error {
        ToolDisplayState::Error
    } else {
        ToolDisplayState::Completed
    };
    ToolDisplay {
        label: tool_label(tool_name, None),
        body: result_body(tool_name, result),
        state,
    }
}

/// 为携带结构化渲染的工具补齐完成态 UI。
pub fn completed_render_spec(tool_name: &str, spec: RenderSpec, result: &ToolResult) -> RenderSpec {
    if tool_name == "agent" {
        agent_done_render_spec(spec, result)
    } else {
        spec
    }
}

fn tool_label(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    let action = match tool_name {
        "shell" => "Bash",
        "readFile" => "Read",
        "writeFile" => "Write",
        "editFile" => "Edit",
        "findFiles" => "Glob",
        "grep" => "Search",
        "apply_patch" | "applyPatch" => "Patch",
        "agent" => "Task",
        other => other,
    };

    if tool_name == "agent" {
        if let Some(description) = arguments
            .and_then(|value| value["description"].as_str())
            .filter(|description| !description.trim().is_empty())
        {
            return format!("{action}({})", compact_inline(description, 56));
        }
        return action.into();
    }

    if let Some(target) = tool_primary_target(tool_name, arguments) {
        let target = target.strip_prefix("$ ").unwrap_or(&target);
        return format!("{action}({})", compact_inline(target, 56));
    }

    action.into()
}

fn request_body(tool_name: &str, arguments: &serde_json::Value) -> String {
    if tool_name == "agent" {
        let mut lines = Vec::new();
        if let Some(subagent_type) = arguments["subagent_type"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("subagent: {subagent_type}"));
        }
        if let Some(prompt) = arguments["prompt"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("prompt: {}", compact_inline(prompt, 180)));
        }
        return if lines.is_empty() {
            "agent".into()
        } else {
            lines.join("\n")
        };
    }

    if let Some(summary) = tool_primary_target(tool_name, Some(arguments)) {
        return summary;
    }

    let args = serde_json::to_string(arguments).unwrap_or_default();
    if args.is_empty() || args == "{}" {
        String::new()
    } else {
        compact_inline(&args, 220)
    }
}

fn result_body(tool_name: &str, result: &ToolResult) -> String {
    if result.is_error {
        let error = result
            .error
            .clone()
            .filter(|error| !error.trim().is_empty())
            .unwrap_or_else(|| result.content.clone());
        return prefixed_lines("error", &error, 8);
    }

    let content = result.content.trim();
    if content.is_empty() {
        return "⎿ done".into();
    }

    match tool_name {
        "readFile" => format!("⎿ read {} line(s)", content.lines().count().max(1)),
        "findFiles" => prefixed_lines("matched files", content, 8),
        "grep" => prefixed_lines("matches", content, 10),
        "writeFile" | "editFile" | "apply_patch" | "applyPatch" => {
            prefixed_lines("result", content, 12)
        },
        _ => prefixed_lines("output", content, 16),
    }
}

fn tool_primary_target(tool_name: &str, arguments: Option<&serde_json::Value>) -> Option<String> {
    let args = arguments?;
    match tool_name {
        "shell" => args["command"].as_str().map(|value| format!("$ {value}")),
        "readFile" | "writeFile" | "editFile" => args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .map(str::to_string),
        "findFiles" => args["pattern"]
            .as_str()
            .or_else(|| args["glob"].as_str())
            .map(|pattern| format!("pattern: {pattern}")),
        "grep" => {
            let pattern = args["pattern"]
                .as_str()
                .or_else(|| args["query"].as_str())
                .unwrap_or_default();
            let path = args["path"]
                .as_str()
                .or_else(|| args["glob"].as_str())
                .unwrap_or_default();
            match (pattern.is_empty(), path.is_empty()) {
                (true, true) => None,
                (false, true) => Some(format!("pattern: {pattern}")),
                (true, false) => Some(path.to_string()),
                (false, false) => Some(format!("{pattern} in {path}")),
            }
        },
        "apply_patch" | "applyPatch" => Some("workspace patch".into()),
        _ => None,
    }
}

fn prefixed_lines(label: &str, text: &str, max_lines: usize) -> String {
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return "⎿ done".into();
    }

    let mut output = Vec::new();
    output.push(format!("⎿ {label}: {}", lines.len()));
    for line in lines.iter().take(max_lines) {
        output.push(format!("⎿ {}", compact_inline(line, 180)));
    }
    if lines.len() > max_lines {
        output.push(format!("⎿ … {} more", lines.len() - max_lines));
    }
    output.join("\n")
}

pub(super) fn compact_inline(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

fn agent_done_render_spec(spec: RenderSpec, result: &ToolResult) -> RenderSpec {
    let mut children = match spec {
        RenderSpec::Box { children, .. } => children,
        spec => vec![spec],
    };

    if let Some(child_session_id) = result
        .metadata
        .get("child_session_id")
        .and_then(|value| value.as_str())
    {
        children.push(RenderSpec::KeyValue {
            entries: vec![RenderKeyValue {
                key: "session".into(),
                value: child_session_id.into(),
                tone: RenderTone::Muted,
            }],
            tone: RenderTone::Default,
        });
    }

    if !result.content.trim().is_empty() {
        children.push(RenderSpec::Markdown {
            text: result.content.clone(),
            tone: if result.is_error {
                RenderTone::Error
            } else {
                RenderTone::Default
            },
        });
    }

    RenderSpec::Box {
        title: Some(if result.is_error {
            "Failed".into()
        } else {
            "Done".into()
        }),
        tone: if result.is_error {
            RenderTone::Error
        } else {
            RenderTone::Success
        },
        children,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::tool::ToolResult;
    use serde_json::json;

    use super::*;

    fn result(content: &str, is_error: bool) -> ToolResult {
        ToolResult {
            call_id: "call-1".into(),
            content: content.into(),
            is_error,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    #[test]
    fn summarizes_common_tool_arguments() {
        assert_eq!(
            requested("shell", &json!({ "command": "git status --short" })).label,
            "Bash(git status --short)"
        );
        assert_eq!(
            requested("readFile", &json!({ "path": "src/main.rs" })).label,
            "Read(src/main.rs)"
        );
        assert_eq!(
            requested("grep", &json!({ "pattern": "needle", "path": "src" })).label,
            "Search(needle in src)"
        );
        assert_eq!(
            requested("findFiles", &json!({ "pattern": "*.rs" })).label,
            "Glob(pattern: *.rs)"
        );
        assert_eq!(
            requested("editFile", &json!({ "file_path": "src/lib.rs" })).label,
            "Edit(src/lib.rs)"
        );
        assert_eq!(
            requested("agent", &json!({ "description": "inspect tui rendering" })).label,
            "Task(inspect tui rendering)"
        );
    }

    #[test]
    fn summarizes_tool_results_without_dumping_large_output() {
        assert_eq!(completed("shell", &result("", false)).body, "⎿ done");

        let short = completed("shell", &result("ok", false));
        assert!(short.body.contains("⎿ output: 1"));
        assert!(short.body.contains("⎿ ok"));

        let long_output = (0..20)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let long = completed("shell", &result(&long_output, false));
        assert!(long.body.contains("⎿ output: 20"));
        assert!(long.body.contains("⎿ … 4 more"));

        let error = completed("shell", &result("permission denied", true));
        assert_eq!(error.state, ToolDisplayState::Error);
        assert!(error.body.contains("⎿ error: 1"));
        assert!(error.body.contains("permission denied"));
    }
}
