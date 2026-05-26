//! s5r handler 效果模型 — `handler.invoke` 的 `invoke_result.output` 载荷。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `handler.invoke` 成功时的 output 形状（与旧 s6r `CallResponse` 对齐）。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            effect: None,
            data: None,
            error: Some(msg.into()),
            continuations: Vec::new(),
        }
    }

    pub fn effect_name(&self) -> &str {
        self.effect.as_deref().unwrap_or("ok")
    }

    pub fn data_str(&self, key: &str) -> &str {
        self.data
            .as_ref()
            .and_then(|d| d[key].as_str())
            .unwrap_or("")
    }

    pub fn data_value(&self, key: &str) -> Option<&Value> {
        self.data.as_ref().and_then(|d| d.get(key))
    }
}

/// 宿主在收到 [`HandlerResult`] 后调度的后续 `handler.invoke`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
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
                format!("{extension_id}:hook:{on}"),
                serde_json::json!({ "on": on, "input": input }),
            ),
            Self::Tool { name, input } => (
                format!("{extension_id}:tool:{name}"),
                serde_json::json!({ "on": "tool", "name": name, "input": input }),
            ),
        }
    }
}
