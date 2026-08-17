use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{
    PermissionContext, PermissionPolicy, PolicyDecision,
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
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        for path in extract_tool_paths(ctx.tool_input) {
            // globset 构建失败时退化为全部路径敏感（fail-closed）。
            let is_sensitive = self
                .globset
                .as_ref()
                .is_none_or(|globset| path_matches_glob(&path, ctx.working_dir, globset));
            if is_sensitive {
                let rel = path_for_matching(&path, ctx.working_dir);
                return PolicyDecision::Ask {
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
        if let Some(pattern) = sensitive_search_pattern(ctx, self.globset.as_ref()) {
            return PolicyDecision::Ask {
                prompt: format!("Search sensitive path pattern `{pattern}`?"),
                rule_key: Some(format!("sensitive:{pattern}")),
            };
        }
        PolicyDecision::Pass
    }
}

fn sensitive_search_pattern<'a>(
    ctx: &'a PermissionContext<'_>,
    globset: Option<&GlobSet>,
) -> Option<&'a str> {
    let field = match ctx.tool_name {
        "grep" => "glob",
        "glob" => "pattern",
        _ => return None,
    };
    let pattern = ctx.tool_input.get(field)?.as_str()?.trim();
    (!pattern.is_empty()
        && globset
            .is_none_or(|globset| has_unescaped_glob_meta(pattern) || globset.is_match(pattern)))
    .then_some(pattern)
}

fn has_unescaped_glob_meta(pattern: &str) -> bool {
    let mut escaped = false;
    for character in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '*' | '?' | '[' | '{' => return true,
            _ => {},
        }
    }
    false
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
        assert!(matches!(policy.evaluate(&ctx), PolicyDecision::Ask { .. }));
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
                matches!(policy.evaluate(&ctx), PolicyDecision::Ask { .. }),
                "path {path} should trigger ask"
            );
        }
    }

    #[test]
    fn search_patterns_targeting_sensitive_files_trigger_ask() {
        let policy = SensitiveFileAskPolicy::new();
        for (tool_name, field, glob, should_ask) in [
            ("grep", "glob", "**/.npmrc", true),
            ("grep", "glob", ".aws/**", true),
            ("grep", "glob", "**/*.pem", true),
            ("grep", "glob", "**/.npmr?", true),
            ("grep", "glob", "src/lib.rs", false),
            ("glob", "pattern", "**/.env", true),
        ] {
            let mut input = serde_json::json!({"path": "."});
            input
                .as_object_mut()
                .unwrap()
                .insert(field.into(), serde_json::json!(glob));
            let ctx = PermissionContext {
                tool_name,
                tool_input: &input,
                working_dir: std::path::Path::new("/project"),
                resource_accesses: &[],
                approval_mode: ApprovalMode::Manual,
                tool_selection: None,
            };
            assert_eq!(
                matches!(policy.evaluate(&ctx), PolicyDecision::Ask { .. }),
                should_ask,
                "unexpected decision for {tool_name} pattern {glob}"
            );
        }
    }
}
