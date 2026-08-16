//! Built-in ToolRenderer implementations for the 8 standard tools.
//!
//! 每个工具从 ToolResult.metadata 提取结构化数据，产出语义化 RenderSpec。
//! TUI render 层负责将 RenderSpec 映射为终端着色行。

use std::sync::Arc;

use astrcode_core::tool::{ToolPresentation, ToolResult};

use super::{
    fallback::DefaultToolRenderer,
    tool::{ToolRenderCtx, ToolRenderer, ToolRendererRegistry},
};
use crate::tui::render::{RenderKeyValue, RenderSpec, RenderTone};

// ─── Read ─────────────────────────────────────────────────────────────────

pub struct ReadRenderer;

impl ToolRenderer for ReadRenderer {
    fn tool_name(&self) -> &str {
        "read"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
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
        let text = if file_type == "image" {
            let bytes = result
                .metadata
                .get("bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let size = if bytes >= 1024 {
                format!("{}KB", bytes / 1024)
            } else {
                format!("{bytes}B")
            };
            format!("Read image ({size}) — {path}")
        } else {
            let lines = result.content.lines().count().max(1);
            let suffix = if file_type.is_empty() {
                String::new()
            } else {
                format!(", {file_type}")
            };
            format!("{path} ({lines} lines{suffix})")
        };
        Some(RenderSpec::Text {
            text,
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

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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

/// 构建「预览前 max_preview 行 → … more → summary → Box」的渲染;
/// content 无有效行时回退为单行 summary 文本。
fn preview_summary_box(
    content: &str,
    max_preview: usize,
    more_suffix: &str,
    summary: String,
    tone: RenderTone,
) -> RenderSpec {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return RenderSpec::Text {
            text: summary,
            tone,
        };
    }

    let preview = lines
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
    if lines.len() > max_preview {
        children.push(RenderSpec::Text {
            text: format!("… {} more{}", lines.len() - max_preview, more_suffix),
            tone: RenderTone::Muted,
        });
    }
    children.push(RenderSpec::Text {
        text: summary,
        tone,
    });
    RenderSpec::Box {
        title: None,
        tone: RenderTone::Default,
        children,
    }
}

pub struct ShellRenderer;

impl ToolRenderer for ShellRenderer {
    fn tool_name(&self) -> &str {
        "shell"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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
        Some(preview_summary_box(content, 8, " lines", status, tone))
    }
}

// ─── Grep ─────────────────────────────────────────────────────────────────

pub struct GrepRenderer;

impl ToolRenderer for GrepRenderer {
    fn tool_name(&self) -> &str {
        "grep"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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
        Some(preview_summary_box(
            content,
            6,
            "",
            summary,
            RenderTone::Success,
        ))
    }
}

// ─── Find ─────────────────────────────────────────────────────────────────

pub struct FindRenderer;

impl ToolRenderer for FindRenderer {
    fn tool_name(&self) -> &str {
        "glob"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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
        Some(preview_summary_box(
            content,
            8,
            "",
            summary,
            RenderTone::Success,
        ))
    }
}

// ─── Patch ────────────────────────────────────────────────────────────────

pub struct PatchRenderer;

impl ToolRenderer for PatchRenderer {
    fn tool_name(&self) -> &str {
        "patch"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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

// ─── SwitchMode ───────────────────────────────────────────────────────────

pub struct SwitchModeRenderer;

impl ToolRenderer for SwitchModeRenderer {
    fn tool_name(&self) -> &str {
        "switchMode"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
        if result.is_error {
            return None;
        }
        let gate_status = result.metadata.get("gateStatus").and_then(|v| v.as_str());
        if gate_status != Some("review_pending") {
            return None;
        }
        let plan = result
            .metadata
            .get("planContent")
            .and_then(|v| v.as_str())?;
        let review_pass = result
            .metadata
            .get("reviewPass")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let required = result
            .metadata
            .get("requiredPasses")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);

        let subtitle = format!(
            "Review the plan below. Call switchMode again to approve (pass \
             {review_pass}/{required})."
        );

        Some(RenderSpec::Box {
            title: Some("Plan review".into()),
            tone: RenderTone::Accent,
            children: vec![
                RenderSpec::Text {
                    text: subtitle,
                    tone: RenderTone::Default,
                },
                RenderSpec::Markdown {
                    text: plan.to_string(),
                    tone: RenderTone::Default,
                },
            ],
        })
    }
}

// ─── UpsertSessionPlan ────────────────────────────────────────────────────

pub struct UpsertSessionPlanRenderer;

impl ToolRenderer for UpsertSessionPlanRenderer {
    fn tool_name(&self) -> &str {
        "upsertSessionPlan"
    }

    fn render_result(&self, result: &ToolResult, _ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec> {
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

/// presentation intent 对应的内置 renderer 名。
///
/// 工具名未命中注册表时（如扩展工具），按结果声明的 intent 复用内置渲染；
/// `Generic` 或未声明返回 `None`，回退默认摘要。
pub fn intent_renderer_name(result: &ToolResult) -> Option<&'static str> {
    match result.presentation() {
        Some(ToolPresentation::Terminal) => Some("shell"),
        Some(ToolPresentation::Diff) => Some("write"),
        Some(ToolPresentation::Search) => Some("grep"),
        Some(ToolPresentation::Read) => Some("read"),
        Some(ToolPresentation::Generic) | None => None,
    }
}

/// Register all built-in tool renderers.
pub fn register_builtin(tool_reg: &mut ToolRendererRegistry) {
    tool_reg.register(Arc::new(ReadRenderer));
    tool_reg.register(Arc::new(WriteRenderer));
    tool_reg.register(Arc::new(EditRenderer));
    tool_reg.register(Arc::new(FindRenderer));
    tool_reg.register(Arc::new(GrepRenderer));
    tool_reg.register(Arc::new(ShellRenderer));
    tool_reg.register(Arc::new(PatchRenderer));
    tool_reg.register(Arc::new(AgentRenderer));
    tool_reg.register(Arc::new(SwitchModeRenderer));
    tool_reg.register(Arc::new(UpsertSessionPlanRenderer));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_renderer_name_maps_presentation_to_builtin_renderers() {
        let cases = [
            (ToolPresentation::Terminal, Some("shell")),
            (ToolPresentation::Diff, Some("write")),
            (ToolPresentation::Search, Some("grep")),
            (ToolPresentation::Read, Some("read")),
            (ToolPresentation::Generic, None),
        ];
        for (presentation, expected) in cases {
            let result = ToolResult::success("ok").with_presentation(presentation);
            assert_eq!(intent_renderer_name(&result), expected, "{presentation:?}");
        }

        assert_eq!(intent_renderer_name(&ToolResult::success("ok")), None);
    }
}
