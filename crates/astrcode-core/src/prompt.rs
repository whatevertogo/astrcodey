//! Prompt composition traits and types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A named, typed block of prompt content with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSpec {
    /// Unique block name (used for deduplication).
    pub name: String,
    /// Raw content — may contain `{{variable}}` templates.
    pub content: String,
    /// Priority within the layer (lower = earlier in output).
    pub priority: u32,
    /// Which caching layer this block belongs to.
    pub layer: PromptLayer,
    /// Optional conditions that must be met for this block to be included.
    #[serde(default)]
    pub conditions: Vec<BlockCondition>,
    /// Names of blocks this one depends on (must appear before).
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Extra metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Caching layer for prompt blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayer {
    /// Never expires — tool guides, identity, core instructions.
    Stable = 0,
    /// Short TTL (5min) — environment, project context.
    SemiStable = 1,
    /// Medium TTL (5min) — inherited rules, AGENTS.md.
    Inherited = 2,
    /// Never cached — user messages, recent history.
    Dynamic = 3,
}

/// Condition for including a block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockCondition {
    /// Variable name to check.
    pub variable: String,
    /// Required value.
    pub equals: String,
}

/// A resolved prompt block ready for rendering.
#[derive(Debug, Clone)]
pub struct PromptBlock {
    pub name: String,
    pub content: String,
    pub layer: PromptLayer,
    pub priority: u32,
}

/// The final assembled prompt plan.
#[derive(Debug, Clone)]
pub struct PromptPlan {
    /// System prompt blocks (rendered, in order).
    pub system_blocks: Vec<PromptBlock>,
    /// Messages to prepend before user messages.
    pub prepend_messages: Vec<String>,
    /// Messages to append after user messages.
    pub append_messages: Vec<String>,
    /// Extra tool definitions beyond built-in.
    pub extra_tools: Vec<crate::tool::ToolDefinition>,
}

/// Context passed to contributors for template resolution.
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// Working directory.
    pub working_dir: String,
    /// Operating system name.
    pub os: String,
    /// Shell being used.
    pub shell: String,
    /// Current date string.
    pub date: String,
    /// Available tool names (comma-separated).
    pub available_tools: String,
    /// Custom variables set by contributors.
    pub custom: BTreeMap<String, String>,
}

/// The `PromptProvider` trait — implemented by the prompt composer.
#[async_trait::async_trait]
pub trait PromptProvider: Send + Sync {
    /// Assemble the full prompt plan for the current context.
    async fn assemble(&self, context: PromptContext) -> PromptPlan;
}

/// A contributor that produces prompt blocks.
#[async_trait::async_trait]
pub trait PromptContributor: Send + Sync {
    /// Unique contributor identifier.
    fn contributor_id(&self) -> &str;

    /// Version of this contributor's output (for cache invalidation).
    fn cache_version(&self) -> &str;

    /// Fingerprint of inputs (for cache invalidation).
    fn cache_fingerprint(&self, context: &PromptContext) -> String;

    /// Produce block specs for the given context.
    async fn contribute(&self, context: &PromptContext) -> Vec<BlockSpec>;
}
