//! Successful `handler.invoke` output and continuation wire contracts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{HandlerId, HandlerKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuations: Vec<CallContinuation>,
}

impl HandlerResult {
    pub fn ok() -> Self {
        Self {
            ok: true,
            effect: Some("ok".into()),
            data: None,
            error: None,
            continuations: Vec::new(),
        }
    }

    pub fn effect(effect: &str, data: Value) -> Self {
        Self {
            ok: true,
            effect: Some(effect.into()),
            data: Some(data),
            error: None,
            continuations: Vec::new(),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            effect: None,
            data: None,
            error: Some(message.into()),
            continuations: Vec::new(),
        }
    }

    pub fn continue_one_step() -> Self {
        Self::effect("continue_one_step", Value::Null)
    }

    pub fn end_turn() -> Self {
        Self::ok()
    }

    pub fn effect_name(&self) -> &str {
        self.effect.as_deref().unwrap_or("ok")
    }

    pub fn data_str(&self, key: &str) -> &str {
        self.data
            .as_ref()
            .and_then(|data| data[key].as_str())
            .unwrap_or("")
    }

    pub fn data_value(&self, key: &str) -> Option<&Value> {
        self.data.as_ref().and_then(|data| data.get(key))
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

impl CallContinuation {
    pub fn handler_id_for_extension(&self, extension_id: &str) -> (String, Value) {
        match self {
            Self::Hook { on, input } => (
                HandlerId::new(extension_id, HandlerKind::Hook, on).into(),
                serde_json::json!({ "on": on, "input": input }),
            ),
            Self::Tool { name, input } => (
                HandlerId::new(extension_id, HandlerKind::Tool, name).into(),
                serde_json::json!({
                    "on": "tool",
                    "name": name,
                    "input": { "arguments": input }
                }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_effect_contract_is_strict_and_builds_typed_continuations() {
        let continuation = CallContinuation::Tool {
            name: "read".into(),
            input: serde_json::json!({ "path": "README.md" }),
        };
        let (handler_id, input) = continuation.handler_id_for_extension("example");
        assert_eq!(handler_id, "example:tool:read");
        assert_eq!(input["input"]["arguments"]["path"], "README.md");

        assert!(
            serde_json::from_value::<HandlerResult>(serde_json::json!({
                "ok": true,
                "effect": "ok",
                "unexpected": true
            }))
            .is_err()
        );
    }
}
