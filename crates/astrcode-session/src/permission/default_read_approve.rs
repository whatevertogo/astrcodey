use astrcode_core::permission::{PermissionContext, PermissionDecision, PermissionPolicy};

pub struct DefaultReadApprovePolicy;

impl PermissionPolicy for DefaultReadApprovePolicy {
    fn priority(&self) -> u32 {
        130
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if matches!(ctx.tool_name, "read" | "grep" | "glob") {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Pass
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::{ApprovalMode, PermissionContext};

    use super::*;

    #[test]
    fn read_auto_allowed() {
        let input = serde_json::json!({"path": "a.rs"});
        let ctx = PermissionContext {
            tool_name: "read",
            tool_input: &input,
            working_dir: std::path::Path::new("/tmp"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            session_id: "s",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert_eq!(
            DefaultReadApprovePolicy.evaluate(&ctx),
            PermissionDecision::Allow
        );
    }
}
