use super::{
    PermissionContext, PermissionPolicy, PolicyDecision,
    paths::{extract_tool_paths, path_for_matching},
};

pub(super) struct GitPathAskPolicy;

impl PermissionPolicy for GitPathAskPolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        for path in extract_tool_paths(ctx.tool_input) {
            if is_git_metadata_path(&path, ctx.working_dir) {
                return PolicyDecision::Ask {
                    prompt: format!("Access git metadata at `{}`?", path.display()),
                    rule_key: Some("git-path".into()),
                };
            }
        }
        PolicyDecision::Pass
    }
}

fn is_git_metadata_path(path: &std::path::Path, working_dir: &std::path::Path) -> bool {
    let rel = path_for_matching(path, working_dir);
    // 与 configured/sensitive 的 glob 匹配语义不同：这里需在路径任意位置
    // 识别 ".git/" 段，故对 rel 做字符串匹配而非 path_matches_glob。
    rel.contains(".git/") || rel.starts_with(".git") || rel == ".git"
}
