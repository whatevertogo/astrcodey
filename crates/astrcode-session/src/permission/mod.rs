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
pub(crate) use runtime::{
    PermissionChain, PermissionContext, PermissionDecision, PermissionPolicy,
};
pub(crate) use session_approval_history::ApprovalHistoryStore;

/// 根据有效配置构建默认权限链。
///
/// 链构造约定（工具管线唯一入口，经 `TurnToolContext::for_turn`）：
/// - Yolo 全覆盖由链保证：`yolo_mode_approve`（priority 50）先于一切 Ask 策略 （65+）恒 Allow，各
///   Ask 策略不再自行判 Yolo。
/// - 链以显式兜底策略收尾（`fallback_allow`，priority 999）：链本身不做隐式拒绝；
///   `PermissionChain::decide` 的全 Pass → Deny 分支仅兜底无终态策略的链 （如 lifecycle 空链）。
/// - 策略按 priority 升序声明，与 `PermissionChain::new` 的 debug_assert 一致。
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
    fn manual_shell_falls_through_to_ask() {
        let effective = test_effective(ApprovalMode::Manual);
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
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        let decision = chain.decide(&ctx, &history);
        assert!(matches!(decision, PermissionDecision::Ask { .. }));
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
        assert_eq!(chain.decide(&ctx, &history), PermissionDecision::Allow);
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
        assert_eq!(chain.decide(&ctx, &history), PermissionDecision::Allow);
    }
}
