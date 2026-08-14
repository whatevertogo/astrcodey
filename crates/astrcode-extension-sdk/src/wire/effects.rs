//! Successful `handler.invoke` output and continuation wire contracts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerEffect {
    Ok,
    ToolOutcome,
    ToolPlan,
    Block,
    Ask,
    ReplaceToolInput,
    ReplaceMessages,
    AppendMessages,
    ProviderContribution,
    ContinueOneStep,
    PromptContributions,
    CompactContributions,
    HttpResponse,
    CustomEventAck,
    CustomEventRetry,
    CustomEventDeadLetter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderContributionEffect {
    Unchanged {},
    ReplaceMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
    AppendMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
}

/// Strict S5R result data for the provider-contribution prepare phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderContributionData {
    pub contribution_id: String,
    pub effect: ProviderContributionEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerResult {
    pub effect: HandlerEffect,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuations: Vec<CallContinuation>,
}

impl HandlerResult {
    pub fn ok() -> Self {
        Self {
            effect: HandlerEffect::Ok,
            data: Value::Null,
            continuations: Vec::new(),
        }
    }

    pub fn effect(effect: HandlerEffect, data: Value) -> Self {
        Self {
            effect,
            data,
            continuations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallContinuation {
    Hook {
        on: String,
        #[serde(default)]
        input: Value,
    },
    Tool {
        name: String,
        #[serde(default)]
        input: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_effect_contract_is_strict() {
        assert!(
            serde_json::from_value::<HandlerResult>(serde_json::json!({
                "effect": "ok",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ToolOutcome>(serde_json::json!({
                "content": "done",
                "is_error": false,
                "kind": "text"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProviderContributionData>(serde_json::json!({
                "contribution_id": "pending-1",
                "effect": {
                    "message_effect": "append_messages"
                }
            }))
            .is_err(),
            "message-bearing effects require messages"
        );
        assert!(
            serde_json::from_value::<ProviderContributionData>(serde_json::json!({
                "contribution_id": "pending-1",
                "effect": {
                    "message_effect": "unchanged",
                    "messages": []
                }
            }))
            .is_err(),
            "unchanged cannot carry a parallel messages field"
        );
    }
}
