use astrcode_core::permission::PermissionRule;
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{
    PermissionContext, PermissionDecision, PermissionPolicy,
    paths::{extract_tool_paths, path_matches_glob},
};

/// 用户配置规则策略：deny / allow / ask 共享同一匹配逻辑，仅决策与优先级不同。
pub(super) struct ConfiguredPolicy {
    rules: Vec<CompiledRule>,
    effect: ConfiguredEffect,
}

/// 规则命中后的决策类型；同时决定策略优先级（见 `PermissionPolicy::priority`）。
#[derive(Clone, Copy)]
pub(super) enum ConfiguredEffect {
    Deny,
    Allow,
    Ask,
}

struct CompiledRule {
    tool: String,
    pattern: Option<String>,
    path_glob: Option<GlobSet>,
}

impl ConfiguredPolicy {
    pub(super) fn new(rules: &[PermissionRule], effect: ConfiguredEffect) -> Self {
        Self {
            rules: compile_rules(rules),
            effect,
        }
    }
}

fn compile_rules(rules: &[PermissionRule]) -> Vec<CompiledRule> {
    rules
        .iter()
        .map(|rule| {
            let path_glob = rule.path.as_deref().and_then(build_globset);
            CompiledRule {
                tool: rule.tool.clone(),
                pattern: rule.pattern.clone(),
                path_glob,
            }
        })
        .collect()
}

fn build_globset(pattern: &str) -> Option<GlobSet> {
    let glob = Glob::new(pattern).ok()?;
    GlobSetBuilder::new().add(glob).build().ok()
}

fn rule_matches(rule: &CompiledRule, ctx: &PermissionContext<'_>) -> bool {
    if rule.tool != ctx.tool_name && rule.tool != "*" {
        return false;
    }
    if let Some(pattern) = &rule.pattern {
        let haystack = ctx.tool_input.to_string();
        if !haystack.contains(pattern) {
            return false;
        }
    }
    if let Some(globset) = &rule.path_glob {
        let paths = extract_tool_paths(ctx.tool_input);
        if paths.is_empty() {
            return false;
        }
        return paths
            .iter()
            .any(|path| path_matches_glob(path, ctx.working_dir, globset));
    }
    true
}

impl PermissionPolicy for ConfiguredPolicy {
    fn priority(&self) -> u32 {
        match self.effect {
            ConfiguredEffect::Deny => 10,
            ConfiguredEffect::Allow => 60,
            ConfiguredEffect::Ask => 65,
        }
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for rule in &self.rules {
            if rule_matches(rule, ctx) {
                return match self.effect {
                    ConfiguredEffect::Deny => PermissionDecision::Deny {
                        reason: format!("Denied by user rule for tool `{}`", ctx.tool_name),
                    },
                    ConfiguredEffect::Allow => PermissionDecision::Allow,
                    ConfiguredEffect::Ask => PermissionDecision::Ask {
                        prompt: format!("User rule requires approval for tool `{}`", ctx.tool_name),
                        rule_key: Some(format!("configured:{}", rule.tool)),
                    },
                };
            }
        }
        PermissionDecision::Pass
    }
}
