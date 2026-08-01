use super::{PermissionContext, PermissionDecision, PermissionPolicy};

pub(super) struct ShellBroadAccessAskPolicy;

impl PermissionPolicy for ShellBroadAccessAskPolicy {
    fn priority(&self) -> u32 {
        110
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
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
    use astrcode_core::permission::ApprovalMode;

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
            tool_selection: None,
        };
        assert!(matches!(
            ShellBroadAccessAskPolicy.evaluate(&ctx),
            PermissionDecision::Ask { .. }
        ));
    }
}
