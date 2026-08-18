//! Result enums returned by extension hook handlers.

use super::types::{CompactContributions, ProviderContributionId};

/// Request-local message effect attached to a contribution that needs durable-success
/// acknowledgement.
#[derive(Debug, Clone)]
pub enum PreparedProviderEffect {
    Unchanged,
    ReplaceMessages(Vec<crate::llm::LlmMessage>),
    AppendMessages(Vec<crate::llm::LlmMessage>),
}

/// Exact pending extension state and its request-local message effect.
#[derive(Debug, Clone)]
pub struct PreparedProviderContribution {
    contribution_id: ProviderContributionId,
    effect: PreparedProviderEffect,
}

impl PreparedProviderContribution {
    pub fn new(contribution_id: ProviderContributionId, effect: PreparedProviderEffect) -> Self {
        Self {
            contribution_id,
            effect,
        }
    }

    pub fn into_parts(self) -> (ProviderContributionId, PreparedProviderEffect) {
        (self.contribution_id, self.effect)
    }
}

/// Generic hook result.
#[derive(Debug, Clone)]
pub enum HookResult {
    Allow,
    Block { reason: String },
}

/// Tool input transform result.
#[derive(Debug, Clone)]
pub enum ToolInputTransformResult {
    Unchanged,
    Replace { tool_input: serde_json::Value },
}

/// Decision of a single PreToolUse admission handler.
#[derive(Debug, Clone)]
pub enum PreToolUseResult {
    Allow,
    Block {
        reason: String,
    },
    /// Ask the user for approval before executing (extension gate Ask).
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
}

/// One extension admission requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreToolUseRequirement {
    pub prompt: String,
    pub rule_key: Option<String>,
}

/// Combined decision of all matching PreToolUse handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolUseAdmission {
    Allow,
    Ask {
        requirements: Vec<PreToolUseRequirement>,
    },
    Block {
        reason: String,
    },
}

/// PostToolUse hook result.
///
/// `ModifyResult` only replaces the text content of the ToolResult (the `content` field); the
/// other structured fields — `is_error` / `error` / `metadata` / `duration_ms` — are left
/// unchanged.
#[derive(Debug, Clone)]
pub enum PostToolUseResult {
    Allow,
    Block { reason: String },
    ModifyResult { content: String },
}

/// Whether the extension runs another agent step after the LLM ends naturally (no tool call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueAfterStopResult {
    EndTurn,
    ContinueOneStep,
}

/// Provider hook result.
#[derive(Debug, Clone)]
pub enum ProviderResult {
    Allow,
    Block {
        reason: String,
    },
    ReplaceMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
    AppendMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
}

/// PreCompact hook result.
#[derive(Debug, Clone)]
pub enum PreCompactResult {
    Allow,
    Block { reason: String },
    Contributions(CompactContributions),
}

/// User message envelope transform result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageEnvelopeResult {
    Allow,
    ReplaceText { text: String },
    AppendText { text: String },
    Block { reason: String },
}
