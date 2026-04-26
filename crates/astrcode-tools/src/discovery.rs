//! Discovery tools: toolSearch, skillTool.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use astrcode_core::tool::*;

// ─── toolSearch ──────────────────────────────────────────────────────────

pub struct ToolSearchTool {
    pub tool_defs: Arc<RwLock<Vec<ToolDefinition>>>,
}

#[async_trait::async_trait]
impl Tool for ToolSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "toolSearch".into(),
            description: "Search available tools by name or description pattern.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        }
    }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or("").to_lowercase();
        let defs = self.tool_defs.read().await;
        let matches: Vec<String> = defs.iter()
            .filter(|t| t.name.to_lowercase().contains(&pattern) || t.description.to_lowercase().contains(&pattern))
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(matches.len()));
        Ok(ToolResult { call_id: String::new(), content: matches.join("\n"), is_error: false, metadata: meta })
    }
}

// ─── skillTool ───────────────────────────────────────────────────────────

/// Skill loader — delegates to extensions for actual skill resolution.
/// Core provides the tool definition only.
pub struct SkillTool;

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skillTool".into(),
            description: "Load a skill by ID. Skills provide specialized domain instructions.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"skill_id":{"type":"string"}},"required":["skill_id"]}),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let skill_id = args["skill_id"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'skill_id'".into()))?;
        Ok(ToolResult {
            call_id: String::new(),
            content: format!("Skill '{skill_id}' — loading via extension. If no extension handles this, it may not be available."),
            is_error: false,
            metadata: BTreeMap::from([("skill_id".into(), serde_json::json!(skill_id))]),
        })
    }
}
