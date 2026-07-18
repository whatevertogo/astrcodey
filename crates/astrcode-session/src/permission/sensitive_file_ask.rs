use astrcode_core::permission::{
    ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy,
};
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::paths::{extract_tool_paths, path_for_matching};

const SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "**/.ssh/**",
    "**/credentials*",
    "**/secret*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
];

pub struct SensitiveFileAskPolicy {
    globset: Option<GlobSet>,
}

impl Default for SensitiveFileAskPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl SensitiveFileAskPolicy {
    pub fn new() -> Self {
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
        if ctx.approval_mode == ApprovalMode::Yolo {
            return PermissionDecision::Pass;
        }
        for path in extract_tool_paths(ctx.tool_input) {
            let rel = path_for_matching(&path, ctx.working_dir);
            let is_sensitive = self.globset.as_ref().is_none_or(|globset| {
                globset.is_match(&rel) || globset.is_match(path.to_string_lossy().as_ref())
            });
            if is_sensitive {
                return PermissionDecision::Ask {
                    prompt: if self.globset.is_some() {
                        format!("Access sensitive path `{}`?", path.display())
                    } else {
                        format!(
                            "Sensitive-file policy is unavailable; allow access to `{}`?",
                            path.display()
                        )
                    },
                    rule_key: Some(format!("sensitive:{}", rel)),
                };
            }
        }
        PermissionDecision::Pass
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::permission::PermissionContext;

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
            session_id: "s",
            is_child_session: false,
            child_tool_policy: None,
        };
        assert!(matches!(
            policy.evaluate(&ctx),
            PermissionDecision::Ask { .. }
        ));
    }
}
