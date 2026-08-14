use super::{PermissionContext, PermissionPolicy, PolicyDecision};

pub(super) struct SessionToolSelectionPolicy;

impl PermissionPolicy for SessionToolSelectionPolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        let Some(selection) = ctx.tool_selection else {
            return PolicyDecision::Pass;
        };
        if selection.allows(ctx.tool_name) {
            return PolicyDecision::Pass;
        }
        PolicyDecision::Deny {
            reason: format!(
                "Tool '{}' is outside this session's selection",
                ctx.tool_name
            ),
        }
    }
}
