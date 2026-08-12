//! Successful `handler.invoke` output and continuation wire contracts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerEffect {
    Ok,
    ToolOutcome,
    Block,
    ModifiedInput,
    ReplaceMessages,
    AppendMessages,
    ContinueOneStep,
    PromptContributions,
    CompactContributions,
    HttpResponse,
    CustomEventAck,
    CustomEventRetry,
    CustomEventDeadLetter,
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

    pub fn data_value(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
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
    }
}
