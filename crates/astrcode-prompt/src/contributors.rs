//! Built-in prompt contributors. Default prompt text source of truth.

use std::path::{Path, PathBuf};

use astrcode_core::prompt::*;

// ─── Default identity (source of truth) ─────────────────────────────────

pub const DEFAULT_IDENTITY: &str = concat!(
    "You are astrcode, a pragmatic autonomous coding agent. ",
    "Read the local context before acting, make small correct changes, ",
    "and keep going until the requested work is complete and verified. ",
    "Prefer root-cause fixes over surface patches, and prefer the repository's existing patterns ",
    "over new abstractions."
);

const TOOL_GUIDE: &str = concat!(
    "Use tools whenever local inspection, execution, or verification is needed. ",
    "Do not guess file contents, build behavior, or command results.\n\n",
    "Tool use rules:\n",
    "- Prefer readFile/findFiles/grep for repository inspection before shell.\n",
    "- Use shell for builds, tests, git, and commands without a dedicated tool. ",
    "Pass cwd instead of changing directories inside the command.\n",
    "- Use editFile for narrow edits with exact oldStr/newStr. ",
    "oldStr must include enough context to match once. ",
    "Use replaceAll only when replacing every match is intended.\n",
    "- Use writeFile only when creating a file or replacing the full content. ",
    "Set createDirs when parent directories may not exist.\n",
    "- Use apply_patch for multi-file or line-oriented patches.\n",
    "- Use adapter-style camelCase tool parameters: oldStr, newStr, replaceAll, createDirs, ",
    "maxResults, maxMatches, caseInsensitive, outputMode, cwd, timeout.\n",
    "- If a tool fails, read the error, correct the arguments or approach, and continue unless ",
    "the request is truly blocked.\n",
    "- After tool results arrive, continue the task loop until the answer is complete; ",
    "do not stop just because one tool call finished."
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

pub struct IdentityContributor;

#[async_trait::async_trait]
impl PromptContributor for IdentityContributor {
    fn contributor_id(&self) -> &str {
        "identity"
    }

    fn cache_version(&self) -> &str {
        "2"
    }

    fn cache_fingerprint(&self, _: &PromptContext) -> String {
        "identity-v2".into()
    }

    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "identity".into(),
            content: DEFAULT_IDENTITY.into(),
            priority: 100,
            layer: PromptLayer::Stable,
            conditions: vec![],
            dependencies: vec![],
            metadata: Default::default(),
        }]
    }
}

// ─── Environment ────────────────────────────────────────────────────────

pub struct EnvironmentContributor;

#[async_trait::async_trait]
impl PromptContributor for EnvironmentContributor {
    fn contributor_id(&self) -> &str {
        "environment"
    }

    fn cache_version(&self) -> &str {
        "1"
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        format!("env-{}-{}-{}", ctx.os, ctx.shell, ctx.working_dir)
    }
    async fn contribute(&self, ctx: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "environment".into(),
            content: format!(
                "Working directory: {}\nOS: {}\nShell: {}\nDate: {}\nAvailable tools: {}",
                ctx.working_dir, ctx.os, ctx.shell, ctx.date, ctx.available_tools
            ),
            priority: 300,
            layer: PromptLayer::SemiStable,
            conditions: vec![],
            dependencies: vec![],
            metadata: Default::default(),
        }]
    }
}

// ─── AGENTS.md rules ────────────────────────────────────────────────────

pub struct AgentsMdContributor;

#[async_trait::async_trait]
impl PromptContributor for AgentsMdContributor {
    fn contributor_id(&self) -> &str {
        "agents-md"
    }

    fn cache_version(&self) -> &str {
        "2"
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        let files = find_agents_files(Path::new(&ctx.working_dir));
        let fingerprints = files
            .iter()
            .filter_map(|path| {
                let metadata = std::fs::metadata(path).ok()?;
                let modified = metadata.modified().ok()?;
                let modified = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                Some(format!("{}:{modified}", path.display()))
            })
            .collect::<Vec<_>>()
            .join("|");
        format!("agentsmd-v2-{}-{fingerprints}", ctx.working_dir)
    }

    async fn contribute(&self, ctx: &PromptContext) -> Vec<BlockSpec> {
        let files = find_agents_files(Path::new(&ctx.working_dir));
        if files.is_empty() {
            return vec![];
        }

        let mut content = String::from(
            "Repository instructions from AGENTS.md files. Follow deeper files when rules \
             conflict.\n",
        );
        for path in files {
            if let Ok(text) = std::fs::read_to_string(&path) {
                content.push_str("\n--- ");
                content.push_str(&path.display().to_string());
                content.push_str(" ---\n");
                content.push_str(&text);
                if !text.ends_with('\n') {
                    content.push('\n');
                }
            }
        }

        vec![BlockSpec {
            name: "agents-md".into(),
            content,
            priority: 400,
            layer: PromptLayer::Inherited,
            conditions: vec![],
            dependencies: vec![],
            metadata: Default::default(),
        }]
    }
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

// ─── Tool capabilities guide ────────────────────────────────────────────

pub struct CapabilityContributor;

#[async_trait::async_trait]
impl PromptContributor for CapabilityContributor {
    fn contributor_id(&self) -> &str {
        "capability"
    }

    fn cache_version(&self) -> &str {
        "2"
    }

    fn cache_fingerprint(&self, _: &PromptContext) -> String {
        "capability-v2".into()
    }

    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "tool-guide".into(),
            content: TOOL_GUIDE.into(),
            priority: 550,
            layer: PromptLayer::SemiStable,
            conditions: vec![],
            dependencies: vec![],
            metadata: Default::default(),
        }]
    }
}

// ─── Response style ─────────────────────────────────────────────────────

pub struct ResponseStyleContributor;

#[async_trait::async_trait]
impl PromptContributor for ResponseStyleContributor {
    fn contributor_id(&self) -> &str {
        "response-style"
    }

    fn cache_version(&self) -> &str {
        "2"
    }

    fn cache_fingerprint(&self, _: &PromptContext) -> String {
        "style-v2".into()
    }

    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "response-style".into(),
            content: RESPONSE_STYLE.into(),
            priority: 560,
            layer: PromptLayer::Stable,
            conditions: vec![],
            dependencies: vec![],
            metadata: Default::default(),
        }]
    }
}

// ─── System instruction (extension-injectable) ──────────────────────────

pub struct SystemInstructionContributor;

#[async_trait::async_trait]
impl PromptContributor for SystemInstructionContributor {
    fn contributor_id(&self) -> &str {
        "system-instruction"
    }

    fn cache_version(&self) -> &str {
        "1"
    }

    fn cache_fingerprint(&self, _: &PromptContext) -> String {
        "instr-v1".into()
    }

    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![]
    }
}
