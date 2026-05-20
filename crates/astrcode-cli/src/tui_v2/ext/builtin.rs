//! Built-in ToolRenderer implementations for the 8 standard tools.

use std::sync::Arc;

use astrcode_core::{render::RenderSpec, tool::ToolResult};
use astrcode_support::text::compact_inline;

use super::{
    fallback::DefaultToolRenderer,
    message::MessageRendererRegistry,
    tool::{ToolRenderCtx, ToolRenderer, ToolRendererRegistry},
};

macro_rules! simple_renderer {
    ($name:ident, $tool:literal) => {
        pub struct $name;
        impl ToolRenderer for $name {
            fn tool_name(&self) -> &str {
                $tool
            }
            fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
                DefaultToolRenderer.render_call(ctx)
            }
            fn render_result(
                &self,
                result: &ToolResult,
                ctx: &mut ToolRenderCtx,
            ) -> Option<RenderSpec> {
                DefaultToolRenderer.render_result(result, ctx)
            }
        }
    };
}

simple_renderer!(ReadRenderer, "read");
simple_renderer!(WriteRenderer, "write");
simple_renderer!(EditRenderer, "edit");
simple_renderer!(FindRenderer, "find");
simple_renderer!(GrepRenderer, "grep");
simple_renderer!(ShellRenderer, "shell");
simple_renderer!(PatchRenderer, "patch");

/// Agent tool renderer — shows description + subagent_type in call, markdown summary in result.
pub struct AgentRenderer;

impl ToolRenderer for AgentRenderer {
    fn tool_name(&self) -> &str {
        "agent"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        let args = ctx.args;
        let description = args
            .and_then(|a| a["description"].as_str())
            .filter(|s| !s.trim().is_empty());
        let subagent_type = args
            .and_then(|a| a["subagent_type"].as_str())
            .filter(|s| !s.trim().is_empty());
        let label = match (description, subagent_type) {
            (Some(d), Some(t)) => format!("Task({}) [{}]", compact_inline(d, 56), t),
            (Some(d), None) => format!("Task({})", compact_inline(d, 56)),
            (None, Some(t)) => format!("Task [{}]", t),
            (None, None) => "Task".into(),
        };
        RenderSpec::Text {
            text: label,
            tone: Default::default(),
        }
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        use astrcode_core::render::{RenderKeyValue, RenderTone};
        let mut children = Vec::new();
        if let Some(sid) = result
            .metadata
            .get("child_session_id")
            .and_then(|v| v.as_str())
        {
            children.push(RenderSpec::KeyValue {
                entries: vec![RenderKeyValue {
                    key: "session".into(),
                    value: sid.into(),
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
        Some(RenderSpec::Box {
            title: Some(if result.is_error {
                "Failed".into()
            } else {
                "Done".into()
            }),
            tone: if result.is_error {
                RenderTone::Error
            } else {
                astrcode_core::render::RenderTone::Success
            },
            children,
        })
    }
}

/// Register all built-in renderers into the provided registries.
pub fn register_builtin(
    tool_reg: &mut ToolRendererRegistry,
    _msg_reg: &mut MessageRendererRegistry,
) {
    tool_reg.register(Arc::new(ReadRenderer));
    tool_reg.register(Arc::new(WriteRenderer));
    tool_reg.register(Arc::new(EditRenderer));
    tool_reg.register(Arc::new(FindRenderer));
    tool_reg.register(Arc::new(GrepRenderer));
    tool_reg.register(Arc::new(ShellRenderer));
    tool_reg.register(Arc::new(PatchRenderer));
    tool_reg.register(Arc::new(AgentRenderer));
}
