use astrcode_core::tool::access::ResourceAccess;

use super::{PermissionContext, PermissionDecision, PermissionPolicy};

pub(super) struct CwdOutsideWriteAskPolicy;

impl PermissionPolicy for CwdOutsideWriteAskPolicy {
    fn priority(&self) -> u32 {
        120
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
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
