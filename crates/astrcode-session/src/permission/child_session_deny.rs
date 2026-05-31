use astrcode_core::{
    extension::ChildToolPolicy,
    permission::{PermissionContext, PermissionDecision, PermissionPolicy},
};

pub struct ChildSessionDenyPolicy;

impl PermissionPolicy for ChildSessionDenyPolicy {
    fn priority(&self) -> u32 {
        20
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if !ctx.is_child_session {
            return PermissionDecision::Pass;
        }
        let Some(policy) = ctx.child_tool_policy else {
            return PermissionDecision::Pass;
        };
        match policy {
            ChildToolPolicy::Deny { tools } if tools.contains(&ctx.tool_name.to_string()) => {
                PermissionDecision::Deny {
                    reason: format!("Tool '{}' is denied for this child session", ctx.tool_name),
                }
            },
            ChildToolPolicy::Allow { tools } if !tools.contains(&ctx.tool_name.to_string()) => {
                PermissionDecision::Deny {
                    reason: format!(
                        "Tool '{}' is not in the child session allow list",
                        ctx.tool_name
                    ),
                }
            },
            _ => PermissionDecision::Pass,
        }
    }
}
