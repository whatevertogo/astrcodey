//! Tool 前端贡献的线缆契约（宿主 Web/TUI 按此选组件，不发给 LLM）。
//!
//! 扩展在 `Registrar::tool_ui` 注册；宿主在 `ToolCallCompleted.metadata.toolUi`
//! 及 SSE `patchMetadata` 中投影给前端。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// ToolResult / conversation block metadata 键。
pub const TOOL_UI_METADATA_KEY: &str = "toolUi";

/// 当前交互阶段（与 `tool_ui_phase` 常量配合）。
pub const TOOL_UI_PHASE_METADATA_KEY: &str = "toolUiPhase";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolUiWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ToolInputUiWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ToolApprovalUiWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResultUiWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolInputUiWire {
    Schema {
        schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ui_schema: Option<Value>,
    },
    Builtin {
        variant: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolApprovalUiWire {
    /// 内置审批/交互卡片：`questionnaire` | `select` | `confirm` | `danger-confirm` | `diff-apply`
    Builtin { variant: String },
    Schema {
        schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ui_schema: Option<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolResultUiWire {
    Builtin { variant: String },
}

/// Tool UI 等待用户填写时的 `ToolResult.content` status 字段值。
pub const TOOL_UI_AWAITING_USER_INPUT: &str = "awaiting_user_input";

/// 判断 tool result 正文是否处于「等待用户填写 Approval UI」阶段。
pub fn is_awaiting_user_input_content(content: &str) -> bool {
    let trimmed = content.trim();
    if !trimmed.starts_with('{') {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(|status| status.as_str())
                .map(|status| status == TOOL_UI_AWAITING_USER_INPUT)
        })
        .unwrap_or(false)
}

/// 将问卷答案合并为 LLM 可见的 tool result 正文（不含 `status: awaiting_user_input`）。
pub fn complete_questionnaire_content(
    questions: &Value,
    answers: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    serde_json::to_string(&serde_json::json!({
        "questions": questions,
        "answers": answers,
    }))
    .map_err(|error| format!("serialize questionnaire response: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn detects_awaiting_user_input_content() {
        let awaiting = r#"{"status":"awaiting_user_input","questions":[]}"#;
        assert!(is_awaiting_user_input_content(awaiting));
        assert!(!is_awaiting_user_input_content(r#"{"answers":{"q":"a"}}"#));
    }

    #[test]
    fn completes_questionnaire_content() {
        let mut answers = BTreeMap::new();
        answers.insert("Which?".into(), "A".into());
        let content =
            complete_questionnaire_content(&serde_json::json!([]), &answers).expect("serialize");
        assert!(content.contains("\"answers\""));
        assert!(content.contains("Which?"));
    }
}
