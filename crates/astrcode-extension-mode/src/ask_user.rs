//! `askUser` tool — structured multiple-choice questions for the user.
//!
//! 后端在 `Registrar::tool_ui` 注册 `questionnaire` Approval；宿主投影 `metadata.toolUi`。
//! 前端只提供通用 `QuestionnaireApprovalCard`，**不**在前端注册 askUser。

use astrcode_extension_sdk::tool::{
    ExecutionMode, ToolApprovalUiWire, ToolDefinition, ToolOrigin, ToolResult, ToolUiWire,
    TOOL_UI_METADATA_KEY, TOOL_UI_PHASE_METADATA_KEY, tool_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Wire name for the ask-user tool (camelCase for LLM tool calls).
pub const ASK_USER_TOOL_NAME: &str = "askUser";

/// 等待用户填写 Approval UI（execute 已返回，不在线程内阻塞）。
pub const TOOL_UI_AWAITING_USER_INPUT: &str = "awaiting_user_input";

/// 提供该交互 UI 的扩展 id（诊断 / 多扩展并存时路由 resolve）。
pub const ASK_USER_EXTENSION_ID_METADATA_KEY: &str = "extensionId";

pub const MODE_EXTENSION_ID: &str = "astrcode-mode";

/// Short label chip max width.
pub const ASK_USER_HEADER_MAX_LEN: usize = 12;

/// Max questions per call.
pub const ASK_USER_MAX_QUESTIONS: usize = 4;

/// Min/max options per question.
pub const ASK_USER_MIN_OPTIONS: usize = 2;
pub const ASK_USER_MAX_OPTIONS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskUserOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserInput {
    pub questions: Vec<AskUserQuestion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AskUserMetadata>,
}

/// 后端注册：askUser 使用宿主内置 `questionnaire` Approval 卡片。
pub fn ask_user_tool_ui() -> ToolUiWire {
    ToolUiWire {
        input: None,
        approval: Some(ToolApprovalUiWire::Builtin {
            variant: "questionnaire".into(),
        }),
        result: None,
    }
}

pub fn ask_user_tool_ui_map() -> std::collections::HashMap<String, ToolUiWire> {
    std::collections::HashMap::from([(ASK_USER_TOOL_NAME.to_string(), ask_user_tool_ui())])
}

/// JSON Schema for LLM tool parameters (`additionalProperties: false`, camelCase).
pub fn ask_user_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: ASK_USER_TOOL_NAME.into(),
        description: (
            "Ask the user one to four multiple-choice questions to clarify requirements, \
             choose between approaches, or confirm decisions.\n\n\
             Plan mode: use BEFORE finalizing the plan to gather preferences. Do NOT use to ask \
             \"is the plan ready?\" — present the plan via upsertSessionPlan, then confirm exit \
             with askUser or switchMode to code.\n\n\
             Users can always pick Other (custom text). Use multiSelect for non-exclusive choices."
        )
        .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": ASK_USER_MAX_QUESTIONS,
                    "description": "Questions to ask (1-4).",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "Full question text, clear and specific."
                            },
                            "header": {
                                "type": "string",
                                "description": format!("Short chip label (max {ASK_USER_HEADER_MAX_LEN} chars).")
                            },
                            "options": {
                                "type": "array",
                                "minItems": ASK_USER_MIN_OPTIONS,
                                "maxItems": ASK_USER_MAX_OPTIONS,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" },
                                        "preview": {
                                            "type": "string",
                                            "description": "Optional markdown preview (single-select only)."
                                        }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false
                            }
                        },
                        "required": ["question", "header", "options"]
                    }
                },
                "metadata": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source": { "type": "string" }
                    }
                }
            },
            "required": ["questions"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

/// Validate input shape and uniqueness rules before showing UI.
pub fn validate_ask_user_input(input: &AskUserInput) -> Result<(), String> {
    if input.questions.is_empty() {
        return Err("questions must contain at least one item".into());
    }
    if input.questions.len() > ASK_USER_MAX_QUESTIONS {
        return Err(format!(
            "at most {ASK_USER_MAX_QUESTIONS} questions allowed"
        ));
    }

    let mut seen_questions = std::collections::HashSet::new();
    for q in &input.questions {
        if q.header.chars().count() > ASK_USER_HEADER_MAX_LEN {
            return Err(format!(
                "header '{}' exceeds {ASK_USER_HEADER_MAX_LEN} characters",
                q.header
            ));
        }
        if !seen_questions.insert(&q.question) {
            return Err("question texts must be unique".into());
        }
        if q.options.len() < ASK_USER_MIN_OPTIONS || q.options.len() > ASK_USER_MAX_OPTIONS {
            return Err(format!(
                "question '{}' must have {ASK_USER_MIN_OPTIONS}-{ASK_USER_MAX_OPTIONS} options",
                q.question
            ));
        }
        let mut seen_labels = std::collections::HashSet::new();
        for opt in &q.options {
            if !seen_labels.insert(&opt.label) {
                return Err(format!(
                    "option labels must be unique within question '{}'",
                    q.question
                ));
            }
            if q.multi_select && opt.preview.is_some() {
                return Err("preview is not supported for multiSelect questions".into());
            }
        }
    }
    Ok(())
}

/// Extension tool handler：校验参数后立即返回 awaiting；完成走宿主 command。
pub fn handle_ask_user(arguments: Value, call_id: &str) -> Result<ToolResult, String> {
    let input: AskUserInput = serde_json::from_value(arguments)
        .map_err(|e| format!("invalid args for {ASK_USER_TOOL_NAME}: {e}"))?;
    validate_ask_user_input(&input)?;
    Ok(ask_user_awaiting_user_input_result(call_id, &input))
}

/// Phase-1 tool result：宿主按 `metadata.toolUi` 渲染 Approval UI；答案由 HTTP/command 写回。
pub fn ask_user_awaiting_user_input_result(call_id: &str, input: &AskUserInput) -> ToolResult {
    let content = serde_json::to_string(&json!({
        "status": TOOL_UI_AWAITING_USER_INPUT,
        "questions": &input.questions,
    }))
    .unwrap_or_else(|_| "{}".into());

    ToolResult {
        call_id: call_id.to_string(),
        content,
        is_error: false,
        error: None,
        metadata: tool_metadata([
            (TOOL_UI_METADATA_KEY, json!(ask_user_tool_ui())),
            (TOOL_UI_PHASE_METADATA_KEY, json!("approval")),
            (ASK_USER_EXTENSION_ID_METADATA_KEY, json!(MODE_EXTENSION_ID)),
        ]),
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> AskUserInput {
        AskUserInput {
            questions: vec![AskUserQuestion {
                question: "Which approach?".into(),
                header: "Approach".into(),
                options: vec![
                    AskUserOption {
                        label: "A".into(),
                        description: "First".into(),
                        preview: None,
                    },
                    AskUserOption {
                        label: "B".into(),
                        description: "Second".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
            metadata: None,
        }
    }

    #[test]
    fn validate_accepts_minimal_question() {
        assert!(validate_ask_user_input(&sample_input()).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_question_text() {
        let mut input = sample_input();
        input.questions.push(input.questions[0].clone());
        assert!(validate_ask_user_input(&input).is_err());
    }

    #[test]
    fn tool_definition_has_expected_name() {
        assert_eq!(ask_user_tool_definition().name, ASK_USER_TOOL_NAME);
    }
}
