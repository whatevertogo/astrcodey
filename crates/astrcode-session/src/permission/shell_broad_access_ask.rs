use astrcode_core::permission::{
    ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy,
};

pub struct ShellBroadAccessAskPolicy;

impl PermissionPolicy for ShellBroadAccessAskPolicy {
    fn priority(&self) -> u32 {
        110
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            return PermissionDecision::Pass;
        }
        if matches!(ctx.tool_name, "shell" | "terminal") {
            let cmd = ctx
                .tool_input
                .get("command")
                .or_else(|| ctx.tool_input.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return PermissionDecision::Ask {
                prompt: format!("Run shell command?\n{cmd}"),
                rule_key: Some(format!("shell:{}", ctx.tool_name)),
            };
        }
        PermissionDecision::Pass
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::PermissionContext;

    use super::*;

    #[test]
    fn shell_triggers_ask() {
        let input = serde_json::json!({"command": "rm -rf /"});
        let ctx = PermissionContext {
            tool_name: "shell",
            tool_input: &input,
            working_dir: std::path::Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            session_id: "s",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert!(matches!(
            ShellBroadAccessAskPolicy.evaluate(&ctx),
            PermissionDecision::Ask { .. }
        ));
    }
}
