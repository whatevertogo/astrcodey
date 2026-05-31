//! Memory extension configuration from `extensions.astrcode.memory`.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct MemoryConfig {
    /// Maximum index records to retain per scope (user + project each trimmed separately).
    pub max_contexts: usize,
    /// Whether SessionStart auto-extraction runs.
    pub auto_extract: bool,
    /// Whether `memory_save` triggers a background sync of changed session rollouts.
    pub auto_extract_after_save: bool,
    /// Max changed sessions to process per pipeline run.
    pub max_changed_sessions: usize,
    /// Skip sessions whose extracted conversation is shorter than this (characters).
    pub min_conversation_chars: usize,
    /// Delete `contexts/` files older than this many days.
    pub max_context_age_days: u64,
    /// Rank project memories at turn end; inject on the next turn's first LLM request.
    pub inject_project_memories_per_turn: bool,
    /// Max project memories to inject per turn.
    pub max_injected_project_memories: usize,
    /// Minimum relevance score (0–1) for injection.
    pub min_project_memory_score: f64,
    /// Max total characters for injected memory block.
    pub max_injected_memory_chars: usize,
    /// Skip turn-end recall when the exchange text is shorter than this (characters).
    pub min_recall_query_chars: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_contexts: 10,
            auto_extract: true,
            auto_extract_after_save: true,
            max_changed_sessions: 5,
            min_conversation_chars: 200,
            max_context_age_days: 90,
            inject_project_memories_per_turn: true,
            max_injected_project_memories: 5,
            min_project_memory_score: 0.35,
            max_injected_memory_chars: 1500,
            min_recall_query_chars: 12,
        }
    }
}

impl MemoryConfig {
    pub(crate) fn from_extension_config(
        config: &astrcode_extension_sdk::extension::ExtensionConfig,
    ) -> Self {
        config.deserialize().unwrap_or_default()
    }
}
