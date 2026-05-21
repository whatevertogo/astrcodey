//! Built-in ToolRenderer implementations for the 8 standard tools.
//!
//! 每个工具从 ToolResult.metadata 提取结构化数据，产出语义化 RenderSpec。
//! TUI render 层负责将 RenderSpec 映射为终端着色行。

use std::sync::Arc;

use astrcode_core::{
    render::{RenderKeyValue, RenderSpec, RenderTone},
    tool::ToolResult,
};
use astrcode_support::text::compact_inline;

use super::{
    fallback::DefaultToolRenderer,
    message::MessageRendererRegistry,
    tool::{ToolRenderCtx, ToolRenderer, ToolRendererRegistry},
};

// ─── Read ─────────────────────────────────────────────────────────────────

pub struct ReadRenderer;

impl ToolRenderer for ReadRenderer {
    fn tool_name(&self) -> &str {
        "read"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let lines = result.content.lines().count().max(1);
        let path = result
            .metadata
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("file");
        let file_type = result
            .metadata
            .get("fileType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let suffix = if file_type.is_empty() {
            String::new()
        } else {
            format!(", {file_type}")
        };
        Some(RenderSpec::Text {
            text: format!("{path} ({lines} lines{suffix})"),
            tone: RenderTone::Success,
        })
    }
}

// ─── Write ────────────────────────────────────────────────────────────────

pub struct WriteRenderer;

impl ToolRenderer for WriteRenderer {
    fn tool_name(&self) -> &str {
        "write"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let created = result
            .metadata
            .get("created")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 如果有 diff 数据，显示 git-style diff
        if let Some(diff) = result.metadata.get("diff").and_then(|v| v.as_str()) {
            let ins = result
                .metadata
                .get("insertions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let del = result
                .metadata
                .get("deletions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut children = vec![RenderSpec::Diff {
                text: diff.to_string(),
                tone: RenderTone::Default,
            }];
            children.push(RenderSpec::Text {
                text: format!("+{ins} -{del}"),
                tone: RenderTone::Muted,
            });
            return Some(RenderSpec::Box {
                title: None,
                tone: RenderTone::Default,
                children,
            });
        }

        // 新建文件，无 diff
        if created {
            let bytes = result
                .metadata
                .get("newBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(RenderSpec::Text {
                text: format!("created ({bytes} bytes)"),
                tone: RenderTone::Success,
            })
        } else {
            Some(RenderSpec::Text {
                text: result.content.clone(),
                tone: RenderTone::Success,
            })
        }
    }
}

// ─── Edit ─────────────────────────────────────────────────────────────────

pub struct EditRenderer;

impl ToolRenderer for EditRenderer {
    fn tool_name(&self) -> &str {
        "edit"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        // 如果有 diff 数据，显示 git-style diff
        if let Some(diff) = result.metadata.get("diff").and_then(|v| v.as_str()) {
            let ins = result
                .metadata
                .get("insertions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let del = result
                .metadata
                .get("deletions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut children = vec![RenderSpec::Diff {
                text: diff.to_string(),
                tone: RenderTone::Default,
            }];
            children.push(RenderSpec::Text {
                text: format!("+{ins} -{del}"),
                tone: RenderTone::Muted,
            });
            return Some(RenderSpec::Box {
                title: None,
                tone: RenderTone::Default,
                children,
            });
        }
        // 无 diff 时回退到摘要
        let ops = result
            .metadata
            .get("operationCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        Some(RenderSpec::Text {
            text: format!("{ops} edit(s) applied"),
            tone: RenderTone::Success,
        })
    }
}

// ─── Shell ────────────────────────────────────────────────────────────────

pub struct ShellRenderer;

impl ToolRenderer for ShellRenderer {
    fn tool_name(&self) -> &str {
        "shell"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        let exit_code = result
            .metadata
            .get("exitCode")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let duration = result.duration_ms.unwrap_or(0);
        let timed_out = result
            .metadata
            .get("timedOut")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let status = if timed_out {
            "timed out".to_string()
        } else if exit_code == 0 {
            format_duration(duration)
        } else {
            format!("exit {} · {}", exit_code, format_duration(duration))
        };

        let tone = if result.is_error {
            RenderTone::Error
        } else {
            RenderTone::Success
        };

        // 对于有实质输出的命令，截取前几行展示
        let content = result.content.trim();
        let output_lines: Vec<&str> = content.lines().collect();
        if output_lines.is_empty() || (output_lines.len() == 1 && output_lines[0].trim().is_empty())
        {
            return Some(RenderSpec::Text { text: status, tone });
        }

        let max_preview = 8;
        let preview: String = output_lines
            .iter()
            .take(max_preview)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let mut children = vec![RenderSpec::Code {
            language: None,
            text: preview,
            tone: RenderTone::Default,
        }];
        if output_lines.len() > max_preview {
            children.push(RenderSpec::Text {
                text: format!("… {} more lines", output_lines.len() - max_preview),
                tone: RenderTone::Muted,
            });
        }
        children.push(RenderSpec::Text { text: status, tone });
        Some(RenderSpec::Box {
            title: None,
            tone: RenderTone::Default,
            children,
        })
    }
}

// ─── Grep ─────────────────────────────────────────────────────────────────

pub struct GrepRenderer;

impl ToolRenderer for GrepRenderer {
    fn tool_name(&self) -> &str {
        "grep"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let returned = result
            .metadata
            .get("returned")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let has_more = result
            .metadata
            .get("hasMore")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let summary = if has_more {
            format!("{returned}+ matches")
        } else {
            format!("{returned} match(es)")
        };

        // 预览前几条匹配
        let content = result.content.trim();
        let preview_lines: Vec<&str> = content.lines().take(6).collect();
        if preview_lines.is_empty() {
            return Some(RenderSpec::Text {
                text: summary,
                tone: RenderTone::Success,
            });
        }

        let preview = preview_lines.join("\n");
        let total_lines = content.lines().count();
        let mut children = vec![RenderSpec::Code {
            language: None,
            text: preview,
            tone: RenderTone::Default,
        }];
        if total_lines > 6 {
            children.push(RenderSpec::Text {
                text: format!("… {} more", total_lines - 6),
                tone: RenderTone::Muted,
            });
        }
        children.push(RenderSpec::Text {
            text: summary,
            tone: RenderTone::Success,
        });
        Some(RenderSpec::Box {
            title: None,
            tone: RenderTone::Default,
            children,
        })
    }
}

// ─── Find ─────────────────────────────────────────────────────────────────

pub struct FindRenderer;

impl ToolRenderer for FindRenderer {
    fn tool_name(&self) -> &str {
        "find"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let count = result
            .metadata
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let total = result
            .metadata
            .get("totalMatches")
            .and_then(|v| v.as_u64())
            .unwrap_or(count);
        let has_more = result
            .metadata
            .get("hasMore")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let summary = if has_more {
            format!("{count} of {total} files")
        } else {
            format!("{total} file(s)")
        };

        // 预览前几个文件路径
        let content = result.content.trim();
        let preview_lines: Vec<&str> = content.lines().take(8).collect();
        if preview_lines.is_empty() {
            return Some(RenderSpec::Text {
                text: summary,
                tone: RenderTone::Success,
            });
        }

        let preview = preview_lines.join("\n");
        let total_lines = content.lines().count();
        let mut children = vec![RenderSpec::Code {
            language: None,
            text: preview,
            tone: RenderTone::Default,
        }];
        if total_lines > 8 {
            children.push(RenderSpec::Text {
                text: format!("… {} more", total_lines - 8),
                tone: RenderTone::Muted,
            });
        }
        children.push(RenderSpec::Text {
            text: summary,
            tone: RenderTone::Success,
        });
        Some(RenderSpec::Box {
            title: None,
            tone: RenderTone::Default,
            children,
        })
    }
}

// ─── Patch ────────────────────────────────────────────────────────────────

pub struct PatchRenderer;

impl ToolRenderer for PatchRenderer {
    fn tool_name(&self) -> &str {
        "patch"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        // Patch 结果也可能带 diff metadata
        if let Some(diff) = result.metadata.get("diff").and_then(|v| v.as_str()) {
            let ins = result
                .metadata
                .get("insertions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let del = result
                .metadata
                .get("deletions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut children = vec![RenderSpec::Diff {
                text: diff.to_string(),
                tone: RenderTone::Default,
            }];
            children.push(RenderSpec::Text {
                text: format!("+{ins} -{del}"),
                tone: RenderTone::Muted,
            });
            return Some(RenderSpec::Box {
                title: None,
                tone: RenderTone::Default,
                children,
            });
        }
        DefaultToolRenderer.render_result(result, _ctx)
    }
}

// ─── Agent ────────────────────────────────────────────────────────────────

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
                RenderTone::Success
            },
            children,
        })
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

// ─── UpsertSessionPlan ────────────────────────────────────────────────────

pub struct UpsertSessionPlanRenderer;

impl ToolRenderer for UpsertSessionPlanRenderer {
    fn tool_name(&self) -> &str {
        "upsertSessionPlan"
    }

    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec {
        DefaultToolRenderer.render_call(ctx)
    }

    fn render_result(&self, result: &ToolResult, _ctx: &mut ToolRenderCtx) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let plan = result
            .metadata
            .get("planContent")
            .and_then(|v| v.as_str())?;
        let operation = result
            .metadata
            .get("operation")
            .and_then(|v| v.as_str())
            .unwrap_or("updated");
        Some(RenderSpec::Box {
            title: Some(format!("Plan {operation}")),
            tone: RenderTone::Success,
            children: vec![RenderSpec::Markdown {
                text: plan.to_string(),
                tone: RenderTone::Default,
            }],
        })
    }
}

// ─── Registration ─────────────────────────────────────────────────────────

/// Register all built-in renderers into the provided registries.
/// TODO：使用 MessageRenderer 验证端到端渲染
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
    tool_reg.register(Arc::new(UpsertSessionPlanRenderer));
}
