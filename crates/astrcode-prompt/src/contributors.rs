//! Built-in prompt section fillers. Default prompt text source of truth.

use std::{
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::prompt::{PromptContext, PromptSection, SystemPromptParts};
use astrcode_support::hostpaths::astrcode_dir;

// ─── Default identity (source of truth) ─────────────────────────────────

pub const DEFAULT_IDENTITY: &str = concat!(
    "You are astrcode, a pragmatic autonomous coding agent. ",
    "Read the local context before acting, make small correct changes, ",
    "and keep going until the requested work is complete and verified. ",
    "Prefer root-cause fixes over surface patches, and prefer the repository's existing patterns ",
    "over new abstractions."
);

const MAX_IDENTITY_SIZE: usize = 4096;

const FEW_SHOT: &str = concat!(
    "Example pattern for implementation work:\n",
    "User: Fix the failing behavior in this repo.\n",
    "Assistant: Inspect the relevant files and call sites, make the smallest root-cause change, ",
    "run the focused verification, then report changed files and any verification gap."
);

const RESPONSE_STYLE: &str = concat!(
    "Keep code identifiers, file paths, commands, and API names in their original spelling.\n\n",
    "Be direct and evidence-backed:\n",
    "- State what changed and what was verified.\n",
    "- Mention blockers or known verification gaps plainly.\n",
    "- Keep final answers compact; avoid motivational filler.\n",
    "- Do not ask for permission for safe local inspection, edits, tests, or formatting that are ",
    "already within the user's request."
);

pub fn add_identity(parts: &mut SystemPromptParts) {
    let identity =
        load_identity_md(&user_identity_md_path()).unwrap_or_else(|| DEFAULT_IDENTITY.to_string());
    parts.set_identity(identity);
}

pub fn user_identity_md_path() -> PathBuf {
    astrcode_dir().join("IDENTITY.md")
}

pub fn load_identity_md(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let identity = if trimmed.len() > MAX_IDENTITY_SIZE {
        truncate_to_char_boundary(trimmed, MAX_IDENTITY_SIZE)
    } else {
        trimmed
    };
    Some(identity.to_string())
}

fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ─── Fixed section fillers ──────────────────────────────────────────────

pub fn add_plugin_system(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(system_prompts) = ctx.custom.get("system_prompts") {
        parts.push(PromptSection::PluginSystem, system_prompts.clone());
    }
}

pub fn add_environment(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    parts.push(
        PromptSection::Environment,
        format!(
            "Working directory: {}\nOS: {}\nShell: {}\nDate: {}",
            ctx.working_dir, ctx.os, ctx.shell, ctx.date
        ),
    );
}

pub fn add_user_rules(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(rules) = ctx.custom.get("user_rules") {
        parts.push(PromptSection::UserRules, rules.clone());
    }
}

pub fn add_project_rules(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    let files = find_agents_files(Path::new(&ctx.working_dir));
    if files.is_empty() {
        return;
    }

    let mut content = String::from(
        "Repository instructions from AGENTS.md files. Follow deeper files when rules conflict.\n",
    );
    for path in files {
        if let Ok(text) = fs::read_to_string(&path) {
            content.push_str("\n--- ");
            content.push_str(&path.display().to_string());
            content.push_str(" ---\n");
            content.push_str(&text);
            if !text.ends_with('\n') {
                content.push('\n');
            }
        }
    }

    parts.push(PromptSection::ProjectRules, content);
}

pub fn add_skills(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(skills) = ctx.custom.get("skills") {
        parts.push(PromptSection::Skills, skills.clone());
    }
}

pub fn add_agents(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(agents) = ctx.custom.get("agents") {
        parts.push(PromptSection::Agents, agents.clone());
    }
}

pub fn add_few_shot(parts: &mut SystemPromptParts) {
    parts.push(PromptSection::FewShot, FEW_SHOT);
}

pub fn add_response_style(parts: &mut SystemPromptParts) {
    parts.set_response_style(RESPONSE_STYLE);
}

fn find_agents_files(working_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(working_dir);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        current = dir.parent();
    }

    // 从浅到深排列，后出现的更具体指令自然覆盖前面的通用指令。
    dirs.reverse();
    dirs.into_iter()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|path| path.is_file())
        .collect()
}
