use astrcode_core::{
    permission::{ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy},
    tool_access::ResourceAccess,
};

pub struct CwdOutsideWriteAskPolicy;

impl PermissionPolicy for CwdOutsideWriteAskPolicy {
    fn priority(&self) -> u32 {
        120
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            return PermissionDecision::Pass;
        }
        if ctx
            .resource_accesses
            .iter()
            .any(|a| matches!(a, ResourceAccess::All))
        {
            return PermissionDecision::Ask {
                prompt: format!(
                    "Tool `{}` may access paths outside the working directory",
                    ctx.tool_name
                ),
                rule_key: Some("cwd-outside".into()),
            };
        }
        PermissionDecision::Pass
    }
}
