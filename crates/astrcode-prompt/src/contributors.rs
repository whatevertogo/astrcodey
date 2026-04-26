//! Built-in prompt contributors. Default prompt text source of truth.

use astrcode_core::prompt::*;

// ─── Default identity (source of truth) ─────────────────────────────────

pub const DEFAULT_IDENTITY: &str = "\
You are astrcode, a genius-level engineer. Code is your expression — correct, \
maintainable. Thoroughly understand before precisely executing; pursue perfect \
and elegant best practices, root-causing problems rather than patching symptoms.";

pub struct IdentityContributor;

#[async_trait::async_trait]
impl PromptContributor for IdentityContributor {
    fn contributor_id(&self) -> &str { "identity" }
    fn cache_version(&self) -> &str { "1" }
    fn cache_fingerprint(&self, _: &PromptContext) -> String { "identity-v1".into() }
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
    fn contributor_id(&self) -> &str { "environment" }
    fn cache_version(&self) -> &str { "1" }
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
    fn contributor_id(&self) -> &str { "agents-md" }
    fn cache_version(&self) -> &str { "1" }
    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        format!("agentsmd-{}", ctx.working_dir)
    }

    async fn contribute(&self, _ctx: &PromptContext) -> Vec<BlockSpec> {
        // Read AGENTS.md from project and user dirs
        // TODO: Load CLUA.md / AGENTS.md from project root and ~/.astrcode/
        vec![]
    }
}

// ─── Tool capabilities guide ────────────────────────────────────────────

pub struct CapabilityContributor;

#[async_trait::async_trait]
impl PromptContributor for CapabilityContributor {
    fn contributor_id(&self) -> &str { "capability" }
    fn cache_version(&self) -> &str { "1" }
    fn cache_fingerprint(&self, _: &PromptContext) -> String { "capability-v1".into() }
    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "tool-guide".into(),
            content: "\
Use the narrowest tool that can answer the request. Prefer reading existing code \
over guessing. When editing, provide enough context for a unique match. \
For shell commands, explain what the command does before running it.".into(),
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
    fn contributor_id(&self) -> &str { "response-style" }
    fn cache_version(&self) -> &str { "1" }
    fn cache_fingerprint(&self, _: &PromptContext) -> String { "style-v1".into() }
    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> {
        vec![BlockSpec {
            name: "response-style".into(),
            content: "Be concise. Use Chinese for explanations, keep code identifiers in original language. \
                One change per response unless the task requires multiple coordinated edits.".into(),
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
    fn contributor_id(&self) -> &str { "system-instruction" }
    fn cache_version(&self) -> &str { "1" }
    fn cache_fingerprint(&self, _: &PromptContext) -> String { "instr-v1".into() }
    async fn contribute(&self, _: &PromptContext) -> Vec<BlockSpec> { vec![] }
}
