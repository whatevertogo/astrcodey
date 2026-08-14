use std::path::Path;

use astrcode_core::{
    permission::ApprovalMode,
    tool::{SessionToolSelection, access::ResourceAccess},
};

use super::ApprovalHistoryStore;

/// 权限策略的评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
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

/// 整条权限链完成评估后的审批要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRequirement {
    pub prompt: String,
    pub rule_key: Option<String>,
}

/// 整条权限链的终态；与单条策略可返回 `Pass` 的结果分开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionResolution {
    Allow,
    Deny {
        reason: String,
    },
    Ask {
        requirements: Vec<PermissionRequirement>,
    },
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
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision;
}

/// 按声明顺序评估；会话记忆只结算产生它的 Ask，不能跳过后续策略。
///
/// Ask 按声明顺序累积，Deny 覆盖此前的 Ask，Allow 结束评估并提交累积结果。没有
/// terminal Allow 的策略链拒绝执行。
pub(crate) struct PermissionChain {
    policies: Vec<Box<dyn PermissionPolicy>>,
}

impl PermissionChain {
    pub(crate) fn new(policies: Vec<Box<dyn PermissionPolicy>>) -> Self {
        Self { policies }
    }

    pub(crate) fn decide(
        &self,
        ctx: &PermissionContext<'_>,
        history: &ApprovalHistoryStore,
    ) -> PermissionResolution {
        let mut requirements = Vec::new();
        for policy in &self.policies {
            match policy.evaluate(ctx) {
                PolicyDecision::Ask {
                    prompt,
                    rule_key: Some(rule_key),
                } => {
                    if history.is_denied_always(&rule_key) {
                        return PermissionResolution::Deny {
                            reason: format!("Denied by session approval memory ({rule_key})"),
                        };
                    }
                    if history.is_allowed_always(&rule_key) {
                        continue;
                    }
                    requirements.push(PermissionRequirement {
                        prompt,
                        rule_key: Some(rule_key),
                    });
                },
                PolicyDecision::Ask {
                    prompt,
                    rule_key: None,
                } => requirements.push(PermissionRequirement {
                    prompt,
                    rule_key: None,
                }),
                PolicyDecision::Deny { reason } => {
                    return PermissionResolution::Deny { reason };
                },
                PolicyDecision::Allow if requirements.is_empty() => {
                    return PermissionResolution::Allow;
                },
                PolicyDecision::Allow => {
                    return PermissionResolution::Ask { requirements };
                },
                PolicyDecision::Pass => {},
            }
        }
        PermissionResolution::Deny {
            reason: "permission chain has no terminal allow policy".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalDecision;

    use super::*;

    struct FixedPolicy {
        decision: PolicyDecision,
    }

    impl PermissionPolicy for FixedPolicy {
        fn evaluate(&self, _ctx: &PermissionContext<'_>) -> PolicyDecision {
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
    fn asks_accumulate_until_terminal_allow_and_deny_overrides() {
        let input = serde_json::json!({});
        let chain = PermissionChain::new(vec![
            Box::new(FixedPolicy {
                decision: PolicyDecision::Pass,
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Ask {
                    prompt: "first".into(),
                    rule_key: Some("first-rule".into()),
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Ask {
                    prompt: "second".into(),
                    rule_key: None,
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Allow,
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Deny {
                    reason: "after terminal allow".into(),
                },
            }),
        ]);
        let history = ApprovalHistoryStore::default();
        assert_eq!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionResolution::Ask {
                requirements: vec![
                    PermissionRequirement {
                        prompt: "first".into(),
                        rule_key: Some("first-rule".into()),
                    },
                    PermissionRequirement {
                        prompt: "second".into(),
                        rule_key: None,
                    },
                ],
            }
        );

        let chain = PermissionChain::new(vec![
            Box::new(FixedPolicy {
                decision: PolicyDecision::Ask {
                    prompt: "first".into(),
                    rule_key: None,
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Deny {
                    reason: "blocked".into(),
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Allow,
            }),
        ]);
        assert_eq!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionResolution::Deny {
                reason: "blocked".into(),
            }
        );

        let chain = PermissionChain::new(vec![Box::new(FixedPolicy {
            decision: PolicyDecision::Pass,
        })]);
        assert!(matches!(
            chain.decide(&empty_ctx(&input), &ApprovalHistoryStore::default()),
            PermissionResolution::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn remembered_approval_skips_only_its_rule() {
        let input = serde_json::json!({});
        let chain = PermissionChain::new(vec![
            Box::new(FixedPolicy {
                decision: PolicyDecision::Ask {
                    prompt: "first".into(),
                    rule_key: Some("first-rule".into()),
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Ask {
                    prompt: "second".into(),
                    rule_key: Some("second-rule".into()),
                },
            }),
            Box::new(FixedPolicy {
                decision: PolicyDecision::Allow,
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
            PermissionResolution::Ask { requirements }
                if requirements == vec![PermissionRequirement {
                    prompt: "second".into(),
                    rule_key: Some("second-rule".into()),
                }]
        ));

        history
            .record_decision(Some("second-rule"), ApprovalDecision::DenyAlways)
            .await
            .unwrap();
        assert!(matches!(
            chain.decide(&empty_ctx(&input), &history),
            PermissionResolution::Deny { .. }
        ));
    }
}
