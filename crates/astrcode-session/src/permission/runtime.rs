use std::path::Path;

use astrcode_core::{
    permission::ApprovalMode,
    tool::{SessionToolSelection, access::ResourceAccess},
};

use super::ApprovalHistoryStore;

/// 权限策略的评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionDecision {
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
pub(crate) struct PermissionContext<'a> {
    pub tool_name: &'a str,
    pub tool_input: &'a serde_json::Value,
    pub working_dir: &'a Path,
    pub resource_accesses: &'a [ResourceAccess],
    pub approval_mode: ApprovalMode,
    pub tool_selection: Option<&'a SessionToolSelection>,
}

pub(crate) trait PermissionPolicy: Send + Sync {
    fn priority(&self) -> u32;
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision;
}

/// 按 priority 升序评估；会话记忆只结算产生它的 Ask，不能跳过后续策略。
///
/// 未被记忆消解的第一条非 Pass 结果胜出；全部 Pass 时拒绝。
pub(crate) struct PermissionChain {
    policies: Vec<Box<dyn PermissionPolicy>>,
}

impl PermissionChain {
    pub(crate) fn new(mut policies: Vec<Box<dyn PermissionPolicy>>) -> Self {
        // 构造方须按 priority 升序声明（见 build_default_chain 链构造约定）；
        // 排序仅作防御，避免声明顺序与 priority() 漂移成双事实来源。
        debug_assert!(
            policies
                .windows(2)
                .all(|pair| pair[0].priority() <= pair[1].priority()),
            "policies must be declared in ascending priority order"
        );
        policies.sort_by_key(|policy| policy.priority());
        Self { policies }
    }

    pub(crate) fn decide(
        &self,
        ctx: &PermissionContext<'_>,
        history: &ApprovalHistoryStore,
    ) -> PermissionDecision {
        for policy in &self.policies {
            let decision = policy.evaluate(ctx);
            match decision {
                PermissionDecision::Ask {
                    prompt,
                    rule_key: Some(rule_key),
                } => {
                    if history.is_denied_always(&rule_key) {
                        return PermissionDecision::Deny {
                            reason: format!("Denied by session approval memory ({rule_key})"),
                        };
                    }
                    if history.is_allowed_always(&rule_key) {
                        continue;
                    }
                    return PermissionDecision::Ask {
                        prompt,
                        rule_key: Some(rule_key),
                    };
                },
                PermissionDecision::Pass => {},
                decision => return decision,
            }
        }
        PermissionDecision::Deny {
            reason: "no permission policy matched".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalDecision;

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
        let history = ApprovalHistoryStore::default();
        assert_eq!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionDecision::Allow
        );

        let chain = PermissionChain::new(vec![Box::new(FixedPolicy {
            priority: 10,
            decision: PermissionDecision::Pass,
        })]);
        assert!(matches!(
            chain.decide(&empty_ctx(&input), &ApprovalHistoryStore::default()),
            PermissionDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn remembered_approval_skips_only_its_rule() {
        let input = serde_json::json!({});
        let chain = PermissionChain::new(vec![
            Box::new(FixedPolicy {
                priority: 10,
                decision: PermissionDecision::Ask {
                    prompt: "first".into(),
                    rule_key: Some("first-rule".into()),
                },
            }),
            Box::new(FixedPolicy {
                priority: 20,
                decision: PermissionDecision::Ask {
                    prompt: "second".into(),
                    rule_key: Some("second-rule".into()),
                },
            }),
        ]);
        let history = ApprovalHistoryStore::default();
        history.ensure_loaded(None).await.unwrap();
        history
            .record_decision(Some("first-rule"), ApprovalDecision::AllowAlways)
            .await
            .unwrap();
        assert!(matches!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionDecision::Ask {
                rule_key: Some(rule_key),
                ..
            } if rule_key == "second-rule"
        ));

        history
            .record_decision(Some("second-rule"), ApprovalDecision::DenyAlways)
            .await
            .unwrap();
        assert!(matches!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionDecision::Deny { .. }
        ));
    }
}
