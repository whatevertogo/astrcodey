//! Prompt 组装的上下文数据。
//!
//! [`PromptContext`] 携带了构建 prompt 所需的全部运行时信息：
//! 工作目录、可用工具、skill 列表、当前步骤/轮次索引、自定义变量等。
//!
//! # 变量解析层次
//!
//! 模板变量解析遵循优先级顺序：block vars → contributor vars → context global vars → builtin vars。
//! 这种分层设计允许 contributor 在局部覆盖全局变量，同时保留操作系统、日期等内建变量。

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use astrcode_core::CapabilitySpec;
use astrcode_runtime_contract::prompt::SystemPromptInstruction;
use astrcode_support::shell::default_shell_label;
use serde::{Deserialize, Serialize};

/// Prompt 组装的运行时上下文。
///
/// 每次调用 [`PromptComposer::build`](crate::composer::PromptComposer::build) 时传入，
/// 包含当前 agent 循环步骤的所有必要信息。
///
/// # 缓存指纹
///
/// [`contributor_cache_fingerprint`](Self::contributor_cache_fingerprint) 用于检测上下文变化，
/// 当指纹改变时 contributor 缓存失效，需要重新收集贡献。

#[derive(Clone, Debug, Default)]
pub struct PromptContext {
    pub working_dir: String,
    pub tool_names: Vec<String>,
    pub capability_specs: Vec<CapabilitySpec>,
    pub system_prompt_instructions: Vec<SystemPromptInstruction>,
    pub agent_profiles: Vec<PromptAgentProfileSummary>,
    pub skills: Vec<PromptSkillSummary>,
    pub step_index: usize,
    pub turn_index: usize,
    pub vars: HashMap<String, String>,
}

/// Prompt 侧的轻量 agent profile 摘要。
///
/// prompt 组装只需要稳定的 profile id 与描述，不依赖 runtime loader 的完整模型，
/// 这样可以把动态 profile 索引注入 prompt，同时保持 crate 边界清晰。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptAgentProfileSummary {
    pub id: String,
    pub description: String,
}

impl PromptAgentProfileSummary {
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

/// Prompt 侧的轻量 skill 摘要。
///
/// prompt 组装只需要稳定的 skill id 与描述，不需要依赖 skill loader 的完整模型，
/// 这样可以保持 adapter-prompt 的编译隔离。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptSkillSummary {
    pub id: String,
    pub description: String,
}

impl PromptSkillSummary {
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

impl PromptContext {
    /// 解析全局变量。
    ///
    /// 支持内建映射（如 `project.working_dir`、`tools.names`）和自定义 `vars` 字典。
    /// 这是模板变量解析链中的第三优先级（低于 block vars 和 contributor vars）。
    pub fn resolve_global_var(&self, key: &str) -> Option<String> {
        match key {
            "project.working_dir" => Some(self.working_dir.clone()),
            "tools.names" => Some(self.tool_names.join(", ")),
            "run.step_index" => Some(self.step_index.to_string()),
            "run.turn_index" => Some(self.turn_index.to_string()),
            _ => self.vars.get(key).cloned(),
        }
    }

    /// 解析内建变量。
    ///
    /// 提供操作系统、当前日期/时间等运行时信息。
    /// 这是模板变量解析链中的最低优先级，仅当其他层都未命中时回退到此。
    pub fn resolve_builtin_var(&self, key: &str) -> Option<String> {
        match key {
            "env.os" => Some(std::env::consts::OS.to_string()),
            // Keep prompt-side shell detection aligned with the shell tool so the model sees
            // which executor will actually run commands before it decides to call `shell`.
            "env.shell" => Some(prompt_default_shell()),
            "run.date" => Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            "run.time" => Some(chrono::Local::now().format("%H:%M:%S").to_string()),
            _ => None,
        }
    }

    /// 获取最近一次用户消息内容。
    ///
    /// 从 `vars["turn.user_message"]` 中读取，用于需要引用用户最新输入的 prompt 块。
    pub fn latest_user_message(&self) -> Option<&str> {
        self.vars.get("turn.user_message").map(String::as_str)
    }

    /// 计算上下文的缓存指纹。
    ///
    /// 基于工作目录、工具列表、能力定义、system prompt 指令、skill 列表和自定义变量
    /// 生成哈希值。当指纹变化时，contributor 缓存应失效。
    pub fn contributor_cache_fingerprint(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.working_dir.hash(&mut hasher);
        self.tool_names.hash(&mut hasher);
        serde_json::to_string(&self.capability_specs)
            .expect("capability specs should serialize")
            .hash(&mut hasher);
        serde_json::to_string(&self.system_prompt_instructions)
            .expect("system prompt instructions should serialize")
            .hash(&mut hasher);
        serde_json::to_string(&self.agent_profiles)
            .expect("agent profiles should serialize")
            .hash(&mut hasher);
        serde_json::to_string(&self.skills)
            .expect("skills should serialize")
            .hash(&mut hasher);

        let mut vars = self.vars.iter().collect::<Vec<_>>();
        vars.sort_by(|left, right| left.0.cmp(right.0));
        for (key, value) in vars {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }

        format!("{:x}", hasher.finish())
    }

    /// 获取配置版本号（用于分层缓存的指纹计算）。
    ///
    /// 当前返回 `None`，未来可从配置文件或运行时状态中读取版本号。
    /// 当配置（如 AGENTS.md、全局规则）变化时，此版本号应递增。
    pub fn config_version(&self) -> Option<&str> {
        self.vars.get("config.version").map(String::as_str)
    }
}

fn prompt_default_shell() -> String {
    default_shell_label()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_global_and_builtin_vars() {
        let mut ctx = PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec!["shell".to_string(), "grep".to_string()],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 1,
            turn_index: 2,
            vars: HashMap::new(),
        };
        ctx.vars
            .insert("project.name".to_string(), "demo".to_string());

        assert_eq!(
            ctx.resolve_global_var("project.working_dir").as_deref(),
            Some("/workspace/demo")
        );
        assert_eq!(
            ctx.resolve_global_var("tools.names").as_deref(),
            Some("shell, grep")
        );
        assert_eq!(
            ctx.resolve_global_var("project.name").as_deref(),
            Some("demo")
        );
        assert_eq!(
            ctx.resolve_builtin_var("env.os").as_deref(),
            Some(std::env::consts::OS)
        );
        assert_eq!(
            ctx.resolve_builtin_var("env.shell").as_deref(),
            Some(prompt_default_shell().as_str())
        );
        assert!(ctx.resolve_builtin_var("run.date").is_some());
    }
}
