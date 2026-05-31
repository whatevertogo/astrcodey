//! Tool Gate 权限类型：审批模式、策略决策、用户审批决议。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    extension::ChildToolPolicy,
    tool_access::ResourceAccess,
};

/// 工具审批模式（全局配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// 默认：敏感操作需用户确认。
    #[default]
    Manual,
    /// 跳过 Ask 类策略，自动 Allow。
    Yolo,
}

impl ApprovalMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }
}

/// 权限链单条策略的评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
    /// 本策略不决策，交给下一条。
    Pass,
}

/// 用户对挂起审批的决议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    DenyOnce,
    AllowAlways,
    DenyAlways,
}

impl ApprovalDecision {
    pub fn allows(&self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// 审批请求来源（扩展 PreToolUse::Ask 或 Core PermissionChain）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalSource {
    Extension,
    Core,
}

/// 传给权限策略的上下文。
#[derive(Debug, Clone)]
pub struct PermissionContext<'a> {
    pub tool_name: &'a str,
    pub tool_input: &'a serde_json::Value,
    pub working_dir: &'a Path,
    pub resource_accesses: &'a [ResourceAccess],
    pub approval_mode: ApprovalMode,
    pub session_id: &'a str,
    pub is_child_session: bool,
    pub child_tool_policy: Option<&'a ChildToolPolicy>,
}

/// 单条权限策略。
pub trait PermissionPolicy: Send + Sync {
    fn priority(&self) -> u32;
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision;
}

/// 按 priority 升序评估，第一条非 Pass 结果胜出。
pub struct PermissionChain {
    policies: Vec<Box<dyn PermissionPolicy>>,
}

impl PermissionChain {
    pub fn new(mut policies: Vec<Box<dyn PermissionPolicy>>) -> Self {
        policies.sort_by_key(|p| p.priority());
        Self { policies }
    }

    pub fn decide(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for policy in &self.policies {
            let decision = policy.evaluate(ctx);
            if !matches!(decision, PermissionDecision::Pass) {
                return decision;
            }
        }
        PermissionDecision::Pass
    }
}

/// 用户配置的权限规则（第三期 DSL）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionsSection {
    #[serde(default)]
    pub deny: Vec<PermissionRule>,
    #[serde(default)]
    pub ask: Vec<PermissionRule>,
    #[serde(default)]
    pub allow: Vec<PermissionRule>,
}

/// 单条权限规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedPolicy {
        priority: u32,
        decision: PermissionDecision,
    }

    impl PermissionPolicy for FixedPolicy {
        fn priority(&self) -> u32 {
            self.priority
        }

        fn evaluate(&self, _ctx: &PermissionContext<'_>) -> PermissionDecision {
            self.decision.clone()
        }
    }

    fn empty_ctx<'a>(input: &'a serde_json::Value) -> PermissionContext<'a> {
        PermissionContext {
            tool_name: "shell",
            tool_input: input,
            working_dir: Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            session_id: "s1",
            is_child_session: false,
            child_tool_policy: None,
        }
    }

    #[test]
    fn chain_first_non_pass_wins() {
        let input = serde_json::json!({});
        let chain = PermissionChain::new(vec![
            Box::new(FixedPolicy {
                priority: 10,
                decision: PermissionDecision::Pass,
            }),
            Box::new(FixedPolicy {
                priority: 20,
                decision: PermissionDecision::Allow,
            }),
            Box::new(FixedPolicy {
                priority: 30,
                decision: PermissionDecision::Deny {
                    reason: "never".into(),
                },
            }),
        ]);
        assert_eq!(chain.decide(&empty_ctx(&input)), PermissionDecision::Allow);
    }

    #[test]
    fn approval_mode_parse() {
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::parse("MANUAL"), Some(ApprovalMode::Manual));
        assert!(ApprovalMode::parse("unknown").is_none());
    }
}
