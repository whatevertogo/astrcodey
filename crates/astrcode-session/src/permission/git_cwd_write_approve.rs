use std::path::Path;

use astrcode_core::permission::{PermissionContext, PermissionDecision, PermissionPolicy};

use super::paths::extract_tool_paths;

pub struct GitCwdWriteApprovePolicy;

impl PermissionPolicy for GitCwdWriteApprovePolicy {
    fn priority(&self) -> u32 {
        140
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if !matches!(ctx.tool_name, "write" | "edit" | "patch") {
            return PermissionDecision::Pass;
        }
        let paths = extract_tool_paths(ctx.tool_input);
        if paths.is_empty() {
            return PermissionDecision::Pass;
        }
        let all_in_cwd = paths.iter().all(|p| {
            let resolved = resolve_relative(ctx.working_dir, p);
            is_within(resolved.as_path(), ctx.working_dir)
        });
        if all_in_cwd {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Pass
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

fn is_within(path: &Path, working_dir: &Path) -> bool {
    astrcode_support::hostpaths::is_path_within(path, working_dir)
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::{ApprovalMode, PermissionContext};

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
            session_id: "s",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert_eq!(
            GitCwdWriteApprovePolicy.evaluate(&ctx),
            PermissionDecision::Allow
        );
    }
}
