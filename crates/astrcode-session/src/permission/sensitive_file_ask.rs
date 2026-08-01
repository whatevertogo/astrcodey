use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{
    PermissionContext, PermissionDecision, PermissionPolicy,
    paths::{extract_tool_paths, path_for_matching, path_matches_glob},
};

// astrcode-extensions::host_router::workspace::is_sensitive_component
// 有对应的组件匹配定义,修改时需同步。
const SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "**/.ssh/**",
    "**/.git/**",
    "**/.aws/**",
    "**/.azure/**",
    "**/.gcloud/**",
    "**/.gitconfig",
    "**/.npmrc",
    "**/credentials*",
    "**/secret*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
];

pub(super) struct SensitiveFileAskPolicy {
    globset: Option<GlobSet>,
}

impl Default for SensitiveFileAskPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl SensitiveFileAskPolicy {
    pub(super) fn new() -> Self {
        let globset = build_sensitive_globset().map_err(|error| {
            tracing::error!(%error, "failed to build sensitive file policy");
            error
        });
        Self {
            globset: globset.ok(),
        }
    }
}

fn build_sensitive_globset() -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in SENSITIVE_PATTERNS {
        builder.add(Glob::new(pattern)?);
    }
    builder.build()
}

impl PermissionPolicy for SensitiveFileAskPolicy {
    fn priority(&self) -> u32 {
        90
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for path in extract_tool_paths(ctx.tool_input) {
            // globset 构建失败时退化为全部路径敏感（fail-closed）。
            let is_sensitive = self
                .globset
                .as_ref()
                .is_none_or(|globset| path_matches_glob(&path, ctx.working_dir, globset));
            if is_sensitive {
                let rel = path_for_matching(&path, ctx.working_dir);
                return PermissionDecision::Ask {
                    prompt: if self.globset.is_some() {
                        format!("Access sensitive path `{}`?", path.display())
                    } else {
                        format!(
                            "Sensitive-file policy is unavailable; allow access to `{}`?",
                            path.display()
                        )
                    },
                    rule_key: Some(format!("sensitive:{rel}")),
                };
            }
        }
        PermissionDecision::Pass
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::ApprovalMode;

    use super::*;

    #[test]
    fn env_file_triggers_ask() {
        let policy = SensitiveFileAskPolicy::new();
        let input = serde_json::json!({"path": ".env"});
        let ctx = PermissionContext {
            tool_name: "read",
            tool_input: &input,
            working_dir: std::path::Path::new("/project"),
            resource_accesses: &[],
            approval_mode: ApprovalMode::Manual,
            tool_selection: None,
        };
        assert!(matches!(
            policy.evaluate(&ctx),
            PermissionDecision::Ask { .. }
        ));
    }

    #[test]
    fn vcs_and_provider_config_paths_trigger_ask() {
        let policy = SensitiveFileAskPolicy::new();
        for path in [
            ".git/HEAD",
            "sub/.git/config",
            ".aws/credentials",
            "sub/.azure/config",
            ".gcloud/application_default_credentials.json",
            ".gitconfig",
            "sub/.npmrc",
        ] {
            let input = serde_json::json!({"path": path});
            let ctx = PermissionContext {
                tool_name: "read",
                tool_input: &input,
                working_dir: std::path::Path::new("/project"),
                resource_accesses: &[],
                approval_mode: ApprovalMode::Manual,
                tool_selection: None,
            };
            assert!(
                matches!(policy.evaluate(&ctx), PermissionDecision::Ask { .. }),
                "path {path} should trigger ask"
            );
        }
    }
}
