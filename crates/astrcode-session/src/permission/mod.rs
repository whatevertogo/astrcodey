//! Tool Gate 权限策略与链组装。

mod configured;
mod default_read_approve;
mod fallback_allow;
mod git_cwd_write_approve;
mod git_path_ask;
mod opaque_resource_ask;
mod paths;
mod process_resource_ask;
mod runtime;
mod sensitive_file_ask;
mod session_approval_history;
mod session_tool_selection;
mod yolo_mode_approve;

use std::{path::Path, sync::Arc};

use astrcode_core::config::{EffectiveConfig, defaults::extension_data_dir};
#[cfg(test)]
pub(crate) use runtime::PermissionRequirement;
pub(crate) use runtime::{
    PermissionChain, PermissionContext, PermissionPolicy, PermissionResolution, PolicyDecision,
};
pub(crate) use session_approval_history::ApprovalHistoryStore;

/// 根据有效配置构建默认权限链。
///
/// 链构造约定（工具管线唯一入口，经 `TurnToolContext::for_turn`）：
/// - vector 声明顺序是策略优先级的唯一事实来源。
/// - Yolo 全覆盖由链保证：`yolo_mode_approve` 先于一切 Ask 策略恒 Allow，各 Ask 策略不再自行判
///   Yolo。
/// - 链以显式 `fallback_allow` 收尾：链本身不做隐式拒绝； `PermissionChain::decide` 的全 Pass →
///   Deny 分支仅兜底无终态策略的链 （如 lifecycle 空链）。
pub(crate) fn build_default_chain(effective: &EffectiveConfig) -> Arc<PermissionChain> {
    let policies: Vec<Box<dyn PermissionPolicy>> = vec![
        Box::new(configured::ConfiguredPolicy::new(
            &effective.permissions.deny,
            configured::ConfiguredEffect::Deny,
        )),
        Box::new(session_tool_selection::SessionToolSelectionPolicy),
        Box::new(yolo_mode_approve::YoloModeApprovePolicy),
        Box::new(configured::ConfiguredPolicy::new(
            &effective.permissions.allow,
            configured::ConfiguredEffect::Allow,
        )),
        Box::new(configured::ConfiguredPolicy::new(
            &effective.permissions.ask,
            configured::ConfiguredEffect::Ask,
        )),
        Box::new(sensitive_file_ask::SensitiveFileAskPolicy::new()),
        Box::new(git_path_ask::GitPathAskPolicy),
        Box::new(process_resource_ask::ProcessResourceAskPolicy),
        Box::new(opaque_resource_ask::OpaqueResourceAskPolicy),
        Box::new(default_read_approve::DefaultReadApprovePolicy),
        Box::new(git_cwd_write_approve::GitCwdWriteApprovePolicy),
        Box::new(fallback_allow::FallbackAllowPolicy),
    ];

    Arc::new(PermissionChain::new(policies))
}

/// 审批挂起超时（5 分钟）。
pub(crate) const APPROVAL_TIMEOUT_SECS: u64 = 300;

/// 从 session 存储目录解析审批历史文件路径。
pub(crate) fn approval_history_path(session_store_dir: &Path) -> std::path::PathBuf {
    extension_data_dir(session_store_dir, "astrcode-session").join("approval-history.json")
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings},
        permission::ApprovalMode,
    };

    use super::*;
    use crate::test_support::test_llm_settings;

    fn test_effective(approval_mode: ApprovalMode) -> EffectiveConfig {
        let llm = test_llm_settings();
        EffectiveConfig {
            llm: llm.clone(),
            small_llm: llm,
            context: ContextSettings::default(),
            agent: AgentSettings {
                max_depth: 2,
                tool_max_parallel_calls: 4,
                approval_mode,
            },
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        }
    }

    #[test]
    fn manual_multi_resource_tool_collects_independent_approvals() {
        let effective = test_effective(ApprovalMode::Manual);
        let history = Arc::new(ApprovalHistoryStore::default());
        let chain = build_default_chain(&effective);
        let input = serde_json::json!({"command": "ls"});
        let resources = [
            astrcode_core::tool::access::ResourceAccess::host(
                astrcode_core::tool::access::HostResource::Process,
            ),
            astrcode_core::tool::access::ResourceAccess::Opaque,
        ];
        let ctx = PermissionContext {
            tool_name: "external_tool",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &resources,
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        let decision = chain.decide(&ctx, &history);
        let PermissionResolution::Ask { requirements } = decision else {
            panic!("both independent resources must require approval");
        };
        assert_eq!(
            requirements
                .iter()
                .map(|requirement| requirement.rule_key.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("process-resource:external_tool"),
                Some("opaque-resource:external_tool"),
            ]
        );
    }

    #[test]
    fn yolo_skips_shell_ask() {
        let effective = test_effective(ApprovalMode::Yolo);
        let history = Arc::new(ApprovalHistoryStore::default());
        let chain = build_default_chain(&effective);
        let input = serde_json::json!({"command": "ls"});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[astrcode_core::tool::access::ResourceAccess::host(
                astrcode_core::tool::access::HostResource::Process,
            )],
            approval_mode: ApprovalMode::Yolo,
            tool_selection: None,
        };
        assert_eq!(chain.decide(&ctx, &history), PermissionResolution::Allow);
    }

    #[test]
    fn manual_unknown_tool_allowed_by_fallback() {
        let effective = test_effective(ApprovalMode::Manual);
        let history = Arc::new(ApprovalHistoryStore::default());
        let chain = build_default_chain(&effective);
        let input = serde_json::json!({"query": "test"});
        let ctx = PermissionContext {
            tool_name: "web_search",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        assert_eq!(chain.decide(&ctx, &history), PermissionResolution::Allow);
    }
}
