//! 提示词组装类型。
//!
//! prompt 组装走 pipeline：结构化输入 → `build_system_prompt()` 纯函数 → 完整字符串。
//! 扩展通过 `PromptBuild` 事件追加结构化内容，不写固定 section。

use crate::llm::LlmMessage;

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

/// 已知的 section 标题及其 KV 缓存分组。
///
/// 只列出内置固定 section。宿主或扩展贡献的未知标题统一按 Dynamic 处理。
const SECTION_GROUP_MAP: &[(&str, PromptSectionGroup)] = &[
    ("Identity", PromptSectionGroup::Static),
    ("System", PromptSectionGroup::Static),
    ("Task Guidelines", PromptSectionGroup::Static),
    ("Communication", PromptSectionGroup::Static),
    ("Environment", PromptSectionGroup::Static),
    ("User Rules", PromptSectionGroup::Static),
    ("Project Rules", PromptSectionGroup::Static),
    ("Tool Summary", PromptSectionGroup::Dynamic),
    ("Additional Instructions", PromptSectionGroup::Dynamic),
];

fn section_title_to_group(title: &str) -> PromptSectionGroup {
    SECTION_GROUP_MAP
        .iter()
        .find(|(candidate, _)| *candidate == title)
        .map(|(_, group)| *group)
        .unwrap_or(PromptSectionGroup::Dynamic)
}

fn parse_rendered_sections(text: &str) -> Vec<(String, String)> {
    let text = text.trim();
    if text.is_empty() || !text.starts_with('[') {
        return Vec::new();
    }

    let mut sections = Vec::new();
    let mut current_start = 0;
    let bytes = text.as_bytes();
    let mut index = 1;
    while index < bytes.len().saturating_sub(2) {
        if bytes[index] == b'\n' && bytes[index + 1] == b'\n' && bytes[index + 2] == b'[' {
            let section_text = text[current_start..index].trim();
            if let Some((title, body)) = parse_single_section(section_text) {
                sections.push((title, body));
            }
            current_start = index + 2;
            index += 3;
        } else {
            index += 1;
        }
    }

    let section_text = text[current_start..].trim();
    if let Some((title, body)) = parse_single_section(section_text) {
        sections.push((title, body));
    }

    sections
}

fn parse_single_section(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if !text.starts_with('[') {
        return None;
    }
    let close = text.find(']')?;
    let title = text[1..close].to_string();
    let body = text[close + 1..].trim().to_string();
    Some((title, body))
}

/// 将已渲染的系统提示词按 KV 缓存分组拆成多条 system message。
///
/// 若无法解析 section 标记，回退为单条 system message。
pub fn system_messages_from_prompt(text: &str) -> Vec<LlmMessage> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let parsed = parse_rendered_sections(trimmed);
    if parsed.is_empty() {
        return vec![LlmMessage::system(text)];
    }

    let mut groups: Vec<(PromptSectionGroup, String)> = Vec::new();
    for (title, body) in &parsed {
        let group = section_title_to_group(title);
        let section_text = format!("[{}]\n{}", title, body);

        if let Some(last) = groups.last_mut() {
            if last.0 == group {
                last.1.push_str("\n\n");
                last.1.push_str(&section_text);
                continue;
            }
        }
        groups.push((group, section_text));
    }

    groups
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(_, text)| LlmMessage::system(text))
        .collect()
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

/// 从宿主环境加载到的 prompt 文件内容。
///
/// 这些字段只是 prompt 输入数据，不规定它们来自磁盘、数据库、远程服务或内存。
#[derive(Debug, Clone, Default)]
pub struct PromptFiles {
    pub identity: Option<String>,
    pub user_rules: Option<String>,
    pub project_rules: Option<String>,
}

/// `PromptFileProvider` trait——由宿主提供 prompt 文件/规则来源。
#[async_trait::async_trait]
pub trait PromptFileProvider: Send + Sync {
    async fn load(&self, working_dir: &str, include_agents_rules: bool) -> PromptFiles;
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

    #[test]
    fn system_messages_from_prompt_splits_known_sections_by_group() {
        let messages = system_messages_from_prompt(
            "[Identity]\n  id\n\n[Tool Summary]\n  tools\n\n[Additional Instructions]\n  extra",
        );

        assert_eq!(messages.len(), 2);
        assert!(messages[0].joined_display_text("\n").contains("Identity"));
        assert!(
            messages[1]
                .joined_display_text("\n")
                .contains("Tool Summary")
        );
        assert!(
            messages[1]
                .joined_display_text("\n")
                .contains("Additional Instructions")
        );
    }

    #[test]
    fn system_messages_from_prompt_falls_back_for_plain_text() {
        let messages = system_messages_from_prompt("plain system");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].joined_display_text("\n"), "plain system");
    }
}
