//! Default fallback ToolRenderer — generic tool summary.

use astrcode_core::{render::RenderSpec, tool::ToolResult};
use astrcode_support::text::compact_inline;

use super::tool::{ToolRenderCtx, ToolRenderer};

/// Default renderer used when no specific ToolRenderer is registered.
pub struct DefaultToolRenderer;

impl ToolRenderer for DefaultToolRenderer {
    fn tool_name(&self) -> &str {
        "__default__"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        let label = tool_label(ctx.tool_name, ctx.args);
        RenderSpec::Text {
            text: label,
            tone: Default::default(),
        }
    }

    fn render_result(&self, result: &ToolResult, ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        let body = result_body(ctx.tool_name, result);
        Some(RenderSpec::Text {
            text: body,
            tone: if result.is_error {
                astrcode_core::render::RenderTone::Error
            } else {
                Default::default()
            },
        })
    }
}

fn tool_label(tool_name: &str, args: Option<&serde_json::Value>) -> String {
    let action = human_action(tool_name);
    if let Some(target) = args.and_then(|a| tool_primary_target(tool_name, a)) {
        let target = target.strip_prefix("$ ").unwrap_or(&target);
        format!("{action}({})", compact_inline(target, 56))
    } else {
        action.to_string()
    }
}

fn human_action(tool_name: &str) -> &str {
    match tool_name {
        "shell" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "find" => "Find",
        "grep" => "Search",
        "patch" => "Patch",
        "agent" => "Task",
        other => other,
    }
}

fn tool_primary_target(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    match tool_name {
        "shell" => args["command"].as_str().map(|v| format!("$ {v}")),
        "read" | "write" | "edit" => args["path"].as_str().map(str::to_string),
        "find" => args["pattern"].as_str().map(|p| format!("pattern: {p}")),
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
        "patch" => Some("workspace patch".into()),
        _ => None,
    }
}

fn result_body(tool_name: &str, result: &ToolResult) -> String {
    if result.is_error {
        let error = result
            .error
            .clone()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| result.content.clone());
        return prefixed_lines("error", &error, 8);
    }
    let content = result.content.trim();
    if content.is_empty() {
        return "⎿ done".into();
    }
    match tool_name {
        "read" => format!("⎿ read {} line(s)", content.lines().count().max(1)),
        "find" => prefixed_lines("matched files", content, 8),
        "grep" => prefixed_lines("matches", content, 10),
        "write" | "edit" | "patch" => prefixed_lines("result", content, 12),
        _ => prefixed_lines("output", content, 16),
    }
}

fn prefixed_lines(label: &str, text: &str, max_lines: usize) -> String {
    let lines: Vec<_> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return "⎿ done".into();
    }
    let mut out = vec![format!("⎿ {label}: {}", lines.len())];
    for line in lines.iter().take(max_lines) {
        out.push(format!("⎿ {}", compact_inline(line, 180)));
    }
    if lines.len() > max_lines {
        out.push(format!("⎿ … {} more", lines.len() - max_lines));
    }
    out.join("\n")
}
