//! 提示词组装类型。
//!
//! prompt 系统只有固定槽位：能力只能把文本放进所属 section，最终渲染为一个
//! 稳定的 system prompt。不要重新引入自由命名 block、priority 或 dependency
//! 机制。

use std::collections::BTreeMap;

/// 固定 system prompt section。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSection {
    PluginSystem,
    Environment,
    UserRules,
    ProjectRules,
    Skills,
    Agents,
    FewShot,
}

/// system prompt 的固定槽位集合。
#[derive(Debug, Clone, Default)]
pub struct SystemPromptParts {
    pub plugin_system: Vec<String>,
    pub identity: Option<String>,
    pub environment: Vec<String>,
    pub user_rules: Vec<String>,
    pub project_rules: Vec<String>,
    pub skills: Vec<String>,
    pub agents: Vec<String>,
    pub few_shot: Vec<String>,
    pub response_style: Option<String>,
}

impl SystemPromptParts {
    pub fn set_identity(&mut self, text: impl Into<String>) {
        self.identity = non_empty(text);
    }

    pub fn set_response_style(&mut self, text: impl Into<String>) {
        self.response_style = non_empty(text);
    }

    pub fn push(&mut self, section: PromptSection, text: impl Into<String>) {
        let Some(text) = non_empty(text) else {
            return;
        };

        match section {
            PromptSection::PluginSystem => self.plugin_system.push(text),
            PromptSection::Environment => self.environment.push(text),
            PromptSection::UserRules => self.user_rules.push(text),
            PromptSection::ProjectRules => self.project_rules.push(text),
            PromptSection::Skills => self.skills.push(text),
            PromptSection::Agents => self.agents.push(text),
            PromptSection::FewShot => self.few_shot.push(text),
        }
    }

    pub fn render_system_prompt(&self) -> String {
        let mut sections = Vec::new();

        push_optional_section(&mut sections, "Identity", self.identity.as_deref());
        push_vec_section(&mut sections, "Environment", &self.environment);
        push_vec_section(&mut sections, "User Rules", &self.user_rules);
        push_vec_section(&mut sections, "Project Rules", &self.project_rules);
        push_vec_section(&mut sections, "Plugin Instructions", &self.plugin_system);
        push_vec_section(&mut sections, "Skills", &self.skills);
        push_vec_section(&mut sections, "Agents", &self.agents);
        push_vec_section(&mut sections, "Few Shot", &self.few_shot);
        push_optional_section(
            &mut sections,
            "Response Style",
            self.response_style.as_deref(),
        );

        sections.join("\n\n")
    }
}

fn non_empty(text: impl Into<String>) -> Option<String> {
    let text = text.into();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn push_optional_section(sections: &mut Vec<String>, title: &str, text: Option<&str>) {
    if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
        sections.push(format!("# {title}\n\n{}", text.trim()));
    }
}

fn push_vec_section(sections: &mut Vec<String>, title: &str, parts: &[String]) {
    let body = parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    if !body.is_empty() {
        sections.push(format!("# {title}\n\n{body}"));
    }
}

/// 最终组装的提示词计划。
#[derive(Debug, Clone)]
pub struct PromptPlan {
    /// 完整 system prompt。为空时调用方不应插入 system message。
    pub system_prompt: Option<String>,
    /// 在用户消息之前插入的消息列表；当前主路径不使用，保留给兼容调用链。
    pub prepend_messages: Vec<String>,
    /// 在用户消息之后追加的消息列表；当前主路径不使用，保留给兼容调用链。
    pub append_messages: Vec<String>,
    /// 额外的工具定义（超出内置工具集的部分）。
    pub extra_tools: Vec<crate::tool::ToolDefinition>,
}

impl PromptPlan {
    pub fn from_system_prompt(system_prompt: String) -> Self {
        Self {
            system_prompt: non_empty(system_prompt),
            prepend_messages: vec![],
            append_messages: vec![],
            extra_tools: vec![],
        }
    }
}

/// 传递给提示词组装器的上下文。
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// 工作目录路径。
    pub working_dir: String,
    /// 操作系统名称。
    pub os: String,
    /// 当前使用的 Shell。
    pub shell: String,
    /// 当前日期字符串。
    pub date: String,
    /// 可用工具名称列表（逗号分隔）。
    pub available_tools: String,
    /// 组装器可选读取的自定义变量。
    pub custom: BTreeMap<String, String>,
}

/// `PromptProvider` trait——由提示词组合器实现。
#[async_trait::async_trait]
pub trait PromptProvider: Send + Sync {
    /// 根据当前上下文组装完整的提示词计划。
    async fn assemble(&self, context: PromptContext) -> PromptPlan;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_plan_omits_blank_system_prompt() {
        let plan = PromptPlan::from_system_prompt("  ".to_string());

        assert!(plan.system_prompt.is_none());
        assert!(plan.prepend_messages.is_empty());
        assert!(plan.append_messages.is_empty());
        assert!(plan.extra_tools.is_empty());
    }

    #[test]
    fn system_prompt_renders_plugin_after_project_rules() {
        let mut parts = SystemPromptParts::default();
        parts.set_identity("identity");
        parts.push(PromptSection::ProjectRules, "project");
        parts.push(PromptSection::PluginSystem, "plugin");

        let rendered = parts.render_system_prompt();

        let identity = rendered.find("# Identity").unwrap();
        let project = rendered.find("# Project Rules").unwrap();
        let plugin = rendered.find("# Plugin Instructions").unwrap();
        assert!(identity < project);
        assert!(project < plugin);
    }
}
