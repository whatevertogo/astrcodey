use astrcode_core::permission::{
    ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy,
};

use super::paths::{extract_tool_paths, path_for_matching};

pub struct GitPathAskPolicy;

impl PermissionPolicy for GitPathAskPolicy {
    fn priority(&self) -> u32 {
        100
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            return PermissionDecision::Pass;
        }
        for path in extract_tool_paths(ctx.tool_input) {
            let rel = path_for_matching(&path, ctx.working_dir);
            if rel.contains(".git/") || rel.starts_with(".git") || rel == ".git" {
                return PermissionDecision::Ask {
                    prompt: format!("Access git metadata at `{}`?", path.display()),
                    rule_key: Some("git-path".into()),
                };
            }
        }
        PermissionDecision::Pass
    }
}
