use std::path::Path;

use astrcode_core::{
    permission::ApprovalMode,
    tool::{SessionToolSelection, access::ResourceAccess},
};

/// 权限策略的评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny {
        reason: String,
    },
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
    /// 当前策略不决策，交给下一条。
    Pass,
}

/// 传给权限策略的 session 运行时上下文。
#[derive(Debug, Clone)]
pub struct PermissionContext<'a> {
    pub tool_name: &'a str,
    pub tool_input: &'a serde_json::Value,
    pub working_dir: &'a Path,
    pub resource_accesses: &'a [ResourceAccess],
    pub approval_mode: ApprovalMode,
    pub tool_selection: Option<&'a SessionToolSelection>,
}

pub trait PermissionPolicy: Send + Sync {
    fn priority(&self) -> u32;
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision;
}

/// 按 priority 升序评估，第一条非 Pass 结果胜出；全部 Pass 时拒绝。
pub struct PermissionChain {
    policies: Vec<Box<dyn PermissionPolicy>>,
}

impl PermissionChain {
    pub fn new(mut policies: Vec<Box<dyn PermissionPolicy>>) -> Self {
        policies.sort_by_key(|policy| policy.priority());
        Self { policies }
    }

    pub fn decide(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for policy in &self.policies {
            let decision = policy.evaluate(ctx);
            if !matches!(decision, PermissionDecision::Pass) {
                return decision;
            }
        }
        PermissionDecision::Deny {
            reason: "no permission policy matched".into(),
        }
    }
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

    fn empty_ctx(input: &serde_json::Value) -> PermissionContext<'_> {
        PermissionContext {
            tool_name: "shell",
            tool_input: input,
            working_dir: Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        }
    }

    #[test]
    fn first_non_pass_wins_and_all_pass_denies() {
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

        let chain = PermissionChain::new(vec![Box::new(FixedPolicy {
            priority: 10,
            decision: PermissionDecision::Pass,
        })]);
        assert!(matches!(
            chain.decide(&empty_ctx(&input)),
            PermissionDecision::Deny { .. }
        ));
    }
}
