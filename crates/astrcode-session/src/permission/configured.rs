use astrcode_core::permission::{
    ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy, PermissionRule,
};
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::paths::{extract_tool_paths, path_for_matching};

pub struct ConfiguredDenyPolicy {
    rules: Vec<CompiledRule>,
}

pub struct ConfiguredAllowPolicy {
    rules: Vec<CompiledRule>,
}

pub struct ConfiguredAskPolicy {
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    tool: String,
    pattern: Option<String>,
    path_glob: Option<GlobSet>,
}

impl ConfiguredDenyPolicy {
    pub fn new(rules: &[PermissionRule]) -> Self {
        Self {
            rules: compile_rules(rules),
        }
    }
}

impl ConfiguredAllowPolicy {
    pub fn new(rules: &[PermissionRule]) -> Self {
        Self {
            rules: compile_rules(rules),
        }
    }
}

impl ConfiguredAskPolicy {
    pub fn new(rules: &[PermissionRule]) -> Self {
        Self {
            rules: compile_rules(rules),
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
        return paths.iter().any(|path| {
            let rel = path_for_matching(path, ctx.working_dir);
            globset.is_match(&rel) || globset.is_match(path.to_string_lossy().as_ref())
        });
    }
    true
}

impl PermissionPolicy for ConfiguredDenyPolicy {
    fn priority(&self) -> u32 {
        10
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for rule in &self.rules {
            if rule_matches(rule, ctx) {
                return PermissionDecision::Deny {
                    reason: format!("Denied by user rule for tool `{}`", ctx.tool_name),
                };
            }
        }
        PermissionDecision::Pass
    }
}

impl PermissionPolicy for ConfiguredAllowPolicy {
    fn priority(&self) -> u32 {
        60
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        for rule in &self.rules {
            if rule_matches(rule, ctx) {
                return PermissionDecision::Allow;
            }
        }
        PermissionDecision::Pass
    }
}

impl PermissionPolicy for ConfiguredAskPolicy {
    fn priority(&self) -> u32 {
        65
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            return PermissionDecision::Pass;
        }
        for rule in &self.rules {
            if rule_matches(rule, ctx) {
                return PermissionDecision::Ask {
                    prompt: format!("User rule requires approval for tool `{}`", ctx.tool_name),
                    rule_key: Some(format!("configured:{}", rule.tool)),
                };
            }
        }
        PermissionDecision::Pass
    }
}
