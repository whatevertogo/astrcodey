use super::{PermissionContext, PermissionDecision, PermissionPolicy};

pub struct SessionToolSelectionPolicy;

impl PermissionPolicy for SessionToolSelectionPolicy {
    fn priority(&self) -> u32 {
        20
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        let Some(selection) = ctx.tool_selection else {
            return PermissionDecision::Pass;
        };
        if selection.allows(ctx.tool_name) {
            return PermissionDecision::Pass;
        }
        PermissionDecision::Deny {
            reason: format!(
                "Tool '{}' is outside this session's selection",
                ctx.tool_name
            ),
        }
    }
}
