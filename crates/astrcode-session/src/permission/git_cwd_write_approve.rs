use std::path::Path;

use astrcode_extension_sdk::hostpaths::is_path_within;

use super::{PermissionContext, PermissionPolicy, PolicyDecision, paths::extract_tool_paths};

pub(super) struct GitCwdWriteApprovePolicy;

impl PermissionPolicy for GitCwdWriteApprovePolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        if !matches!(ctx.tool_name, "write" | "edit" | "patch") {
            return PolicyDecision::Pass;
        }
        let paths = extract_tool_paths(ctx.tool_input);
        if paths.is_empty() {
            return PolicyDecision::Pass;
        }
        let all_in_cwd = paths.iter().all(|p| {
            let resolved = resolve_relative(ctx.working_dir, p);
            is_path_within(&resolved, ctx.working_dir)
        });
        if all_in_cwd {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Pass
        }
    }
}

fn resolve_relative(working_dir: &Path, raw: &Path) -> std::path::PathBuf {
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        working_dir.join(raw)
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalMode;

    use super::*;

    #[test]
    fn write_in_cwd_allowed() {
        let input = serde_json::json!({"path": "src/a.rs", "content": "x"});
        let ctx = PermissionContext {
            tool_name: "write",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        assert_eq!(
            GitCwdWriteApprovePolicy.evaluate(&ctx),
            PolicyDecision::Allow
        );
    }
}
