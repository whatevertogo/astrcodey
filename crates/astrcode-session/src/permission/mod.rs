//! Tool Gate 权限策略与链组装。

mod child_session_deny;
mod configured;
mod cwd_outside_write_ask;
mod default_read_approve;
mod fallback_allow;
mod git_cwd_write_approve;
mod git_path_ask;
mod paths;
mod sensitive_file_ask;
mod session_approval_history;
mod shell_broad_access_ask;
mod yolo_mode_approve;

use std::{path::Path, sync::Arc};

use astrcode_core::{
    config::EffectiveConfig,
    permission::{PermissionChain, PermissionPolicy},
};
pub use session_approval_history::ApprovalHistoryStore;

/// 根据有效配置与会话审批记忆构建默认权限链。
pub fn build_default_chain(
    effective: &EffectiveConfig,
    history: Arc<ApprovalHistoryStore>,
) -> Arc<PermissionChain> {
    let policies: Vec<Box<dyn PermissionPolicy>> = vec![
        Box::new(configured::ConfiguredDenyPolicy::new(
            &effective.permissions.deny,
        )),
        Box::new(child_session_deny::ChildSessionDenyPolicy),
        Box::new(yolo_mode_approve::YoloModeApprovePolicy),
        Box::new(session_approval_history::SessionApprovalHistoryPolicy::new(
            Arc::clone(&history),
        )),
        Box::new(configured::ConfiguredAllowPolicy::new(
            &effective.permissions.allow,
        )),
        Box::new(configured::ConfiguredAskPolicy::new(
            &effective.permissions.ask,
        )),
        Box::new(sensitive_file_ask::SensitiveFileAskPolicy::new()),
        Box::new(git_path_ask::GitPathAskPolicy),
        Box::new(shell_broad_access_ask::ShellBroadAccessAskPolicy),
        Box::new(cwd_outside_write_ask::CwdOutsideWriteAskPolicy),
        Box::new(default_read_approve::DefaultReadApprovePolicy),
        Box::new(git_cwd_write_approve::GitCwdWriteApprovePolicy),
        Box::new(fallback_allow::FallbackAllowPolicy),
    ];

    Arc::new(PermissionChain::new(policies))
}

/// 审批挂起超时（5 分钟）。
pub const APPROVAL_TIMEOUT_SECS: u64 = 300;

/// 从 session 存储目录解析审批历史文件路径。
pub fn approval_history_path(session_store_dir: &Path) -> std::path::PathBuf {
    session_store_dir
        .join("extension_data")
        .join("astrcode-session")
        .join("approval-history.json")
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings},
        permission::{ApprovalMode, PermissionContext, PermissionDecision},
    };

    use super::*;

    fn test_llm() -> LlmSettings {
        LlmSettings {
            provider_kind: "openai".into(),
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            api_mode: astrcode_core::config::raw::OpenAiApiMode::ChatCompletions,
            model_id: "test".into(),
            max_tokens: 1024,
            context_limit: 8192,
            connect_timeout_secs: 30,
            read_timeout_secs: 120,
            max_retries: 3,
            retry_base_delay_ms: 500,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        }
    }

    fn test_effective(approval_mode: ApprovalMode) -> EffectiveConfig {
        EffectiveConfig {
            llm: test_llm(),
            small_llm: test_llm(),
            context: ContextSettings::default(),
            agent: AgentSettings {
                max_depth: 2,
                tool_max_parallel_calls: 4,
                shell_timeout_secs: 120,
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
        let chain = build_default_chain(&effective, history);
        let input = serde_json::json!({"command": "ls"});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            session_id: "s1",
            is_child_session: false,
            child_tool_policy: None,
        };
        let decision = chain.decide(&ctx);
        assert!(matches!(decision, PermissionDecision::Ask { .. }));
    }

    #[test]
    fn yolo_skips_shell_ask() {
        let effective = test_effective(ApprovalMode::Yolo);
        let history = Arc::new(ApprovalHistoryStore::default());
        let chain = build_default_chain(&effective, history);
        let input = serde_json::json!({"command": "ls"});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Yolo,
            session_id: "s1",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert_eq!(chain.decide(&ctx), PermissionDecision::Allow);
    }

    #[test]
    fn manual_unknown_tool_allowed_by_fallback() {
        let effective = test_effective(ApprovalMode::Manual);
        let history = Arc::new(ApprovalHistoryStore::default());
        let chain = build_default_chain(&effective, history);
        let input = serde_json::json!({"query": "test"});
        let ctx = PermissionContext {
            tool_name: "web_search",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            session_id: "s1",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert_eq!(chain.decide(&ctx), PermissionDecision::Allow);
    }
}
