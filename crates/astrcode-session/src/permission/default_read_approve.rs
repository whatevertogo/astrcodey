use super::{PermissionContext, PermissionPolicy, PolicyDecision};

pub(super) struct DefaultReadApprovePolicy;

impl PermissionPolicy for DefaultReadApprovePolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        if matches!(ctx.tool_name, "read" | "grep" | "glob") {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Pass
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalMode;

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
            tool_selection: None,
        };
        assert_eq!(
            DefaultReadApprovePolicy.evaluate(&ctx),
            PolicyDecision::Allow
        );
    }
}
