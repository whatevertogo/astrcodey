use astrcode_core::tool::access::{HostResource, ResourceAccess};

use super::{PermissionContext, PermissionDecision, PermissionPolicy};

pub(super) struct ProcessResourceAskPolicy;

pub(super) fn rule_key(tool_name: &str) -> String {
    format!("process-resource:{tool_name}")
}

impl PermissionPolicy for ProcessResourceAskPolicy {
    fn priority(&self) -> u32 {
        110
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if !ctx
            .resource_accesses
            .iter()
            .any(|access| matches!(access, ResourceAccess::Host(HostResource::Process)))
        {
            return PermissionDecision::Pass;
        }

        let command = ctx
            .tool_input
            .get("command")
            .or_else(|| ctx.tool_input.get("cmd"))
            .and_then(serde_json::Value::as_str)
            .filter(|command| !command.is_empty());
        let prompt = match command {
            Some(command) => format!(
                "Allow tool `{}` to run a process?\n{command}",
                ctx.tool_name
            ),
            None => format!("Allow tool `{}` to control a process?", ctx.tool_name),
        };
        PermissionDecision::Ask {
            prompt,
            rule_key: Some(rule_key(ctx.tool_name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        permission::ApprovalMode,
        tool::access::{HostResource, ResourceAccess},
    };

    use super::*;

    #[test]
    fn every_process_backed_tool_requires_approval() {
        let input = serde_json::json!({"command": "cargo test"});
        let process = [ResourceAccess::host(HostResource::Process)];
        for tool_name in ["shell", "terminal", "run_tests"] {
            let ctx = PermissionContext {
                tool_name,
                tool_input: &input,
                working_dir: std::path::Path::new("/tmp"),
                resource_accesses: &process,
                approval_mode: ApprovalMode::Manual,
                tool_selection: None,
            };
            assert!(matches!(
                ProcessResourceAskPolicy.evaluate(&ctx),
                PermissionDecision::Ask { .. }
            ));
        }
    }
}
