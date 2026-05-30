//! 提示词组装类型。
//!
//! prompt 组装走 pipeline：结构化输入 → `build_system_prompt()` 纯函数 → 完整字符串。
//! 扩展通过 `PromptBuild` 事件追加结构化内容，不写固定 section。

/// 系统提示词 section 的 KV 缓存分组。
///
/// 稳定前缀（Identity → ProjectRules）跨 turn 复用，动态后缀
/// （ToolSummary、Extension blocks、ExtraInstructions）每 turn 刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSectionGroup {
    /// Identity、System、TaskGuidelines、Communication — 跨 session 稳定。
    Static,
    /// Environment、UserRules、ProjectRules — 同项目内稳定。
    SemiStatic,
    /// ToolSummary、ExtensionPrompt、ExtraInstructions — 每次可能不同。
    Dynamic,
}

/// 最终组装的提示词计划。
#[derive(Debug, Clone)]
pub struct PromptPlan {
    pub system_prompt: Option<String>,
}

impl PromptPlan {
    pub fn from_system_prompt(system_prompt: String) -> Self {
        let trimmed = system_prompt.trim();
        Self {
            system_prompt: if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            },
        }
    }
}

/// `PromptProvider` trait——由提示词组合器实现。
#[async_trait::async_trait]
pub trait PromptProvider: Send + Sync {
    async fn assemble(&self, input: SystemPromptInput) -> PromptPlan;
}

// ─── Pipeline 类型 ─────────────────────────────────────────────────────

use crate::tool::{ToolDefinition, ToolPromptMetadata};

/// `build_system_prompt()` 的结构化输入。
#[derive(Debug, Clone)]
pub struct SystemPromptInput {
    pub working_dir: String,
    pub os: String,
    pub shell: String,
    /// GitHub CLI (`gh`) 是否在 PATH 中可用。
    pub gh_cli_available: bool,
    pub identity: Option<String>,
    pub user_rules: Option<String>,
    /// 已加载的 project rules（AGENTS.md 内容）。
    pub project_rules: Option<String>,
    /// 当前 session 固定下来的工具定义快照。
    pub tools: Vec<ToolDefinition>,
    /// 工具的结构化提示词元数据。键为工具名，与 `tools[].name` 匹配。
    pub tool_prompt_metadata: std::collections::HashMap<String, ToolPromptMetadata>,
    /// 扩展贡献的文本块，按 section 归类。
    pub extension_blocks: Vec<ExtensionPromptBlock>,
    /// 额外的系统指令（如子会话 prompt）。
    pub extra_instructions: Option<String>,
}

/// 扩展贡献的文本块，带逻辑分类标签。
#[derive(Debug, Clone)]
pub struct ExtensionPromptBlock {
    pub section: ExtensionSection,
    pub content: String,
}

/// 扩展可贡献文本的逻辑分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionSection {
    PlatformInstructions,
    AdditionalInstructions,
    Skills,
    Agents,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_plan_omits_blank_system_prompt() {
        let plan = PromptPlan::from_system_prompt("  ".to_string());
        assert!(plan.system_prompt.is_none());
    }
}
