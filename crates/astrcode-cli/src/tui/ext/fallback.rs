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

    fn render_result(&self, result: &ToolResult, ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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
        "glob" => prefixed_lines("matched files", content, 8),
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
