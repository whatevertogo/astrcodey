use astrcode_core::tool::access::ResourceAccess;

use super::{PermissionContext, PermissionPolicy, PolicyDecision};

pub(super) struct OpaqueResourceAskPolicy;

pub(super) fn rule_key(tool_name: &str) -> String {
    format!("opaque-resource:{tool_name}")
}

impl PermissionPolicy for OpaqueResourceAskPolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        if ctx
            .resource_accesses
            .iter()
            .any(|access| matches!(access, ResourceAccess::Opaque))
        {
            return PolicyDecision::Ask {
                prompt: format!(
                    "Tool `{}` may perform external side effects that AstrCode cannot scope",
                    ctx.tool_name
                ),
                rule_key: Some(rule_key(ctx.tool_name)),
            };
        }
        PolicyDecision::Pass
    }
}
