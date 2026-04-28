//! 工具发现与技能加载工具。
//!
//! 提供 `toolSearch`（按名称/描述搜索可用工具）和 `skillTool`（按 ID 加载技能）。
//! 这两个工具帮助 LLM 在运行时动态发现可用的工具和技能。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_core::tool::*;
use tokio::sync::RwLock;

// ─── toolSearch ──────────────────────────────────────────────────────────

/// 工具搜索工具，按名称或描述模式匹配已注册的工具。
///
/// 持有当前所有工具定义的共享引用，在执行时按关键词过滤并返回匹配列表。
pub struct ToolSearchTool {
    /// 已注册工具的定义列表（读写锁保护，支持并发读取）
    pub tool_defs: Arc<RwLock<Vec<ToolDefinition>>>,
}

#[async_trait::async_trait]
impl Tool for ToolSearchTool {
    /// 返回 toolSearch 工具的定义，包含名称、描述和参数 schema。
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "toolSearch".into(),
            description: "Search available tools by name or description pattern.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// 执行工具搜索：将 pattern 与所有工具的名称和描述做大小写不敏感的子串匹配。
    ///
    /// - `args["pattern"]`：搜索关键词
    /// - 返回匹配的工具列表，每行格式为 `- name: description`
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or("").to_lowercase();
        let defs = self.tool_defs.read().await;
        let matches: Vec<String> = defs
            .iter()
            .filter(|t| {
                t.name.to_lowercase().contains(&pattern)
                    || t.description.to_lowercase().contains(&pattern)
            })
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(matches.len()));
        Ok(ToolResult {
            call_id: String::new(),
            content: matches.join("\n"),
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: None,
        })
    }
}

// ─── skillTool ───────────────────────────────────────────────────────────

/// 技能加载工具，按 ID 请求加载一个技能。
///
/// 核心层仅提供工具定义和占位执行逻辑，实际的技能解析委托给扩展系统处理。
pub struct SkillTool;

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skillTool".into(),
            description: "Load a skill by ID. Skills provide specialized domain instructions."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"skill_id":{"type":"string"}},"required":["skill_id"]}),
        }
    }

    /// 执行技能加载请求。
    ///
    /// - `args["skill_id"]`：要加载的技能标识符（必填）
    /// - 如果没有扩展处理该技能 ID，返回提示信息
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let skill_id = args["skill_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'skill_id'".into()))?;
        Ok(ToolResult {
            call_id: String::new(),
            content: format!(
                "Skill '{skill_id}' — loading via extension. If no extension handles this, it may \
                 not be available."
            ),
            is_error: false,
            error: None,
            metadata: BTreeMap::from([("skill_id".into(), serde_json::json!(skill_id))]),
            duration_ms: None,
        })
    }
}
