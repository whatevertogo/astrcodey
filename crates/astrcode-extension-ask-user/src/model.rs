use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use astrcode_extension_sdk::tool::{ExecutionMode, ToolDefinition, ToolOrigin};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub(crate) const ASK_USER_TOOL_NAME: &str = "askUser";
pub(crate) const ASK_USER_HEADER_MAX_LEN: usize = 12;
pub(crate) const ASK_USER_MAX_QUESTIONS: usize = 4;
pub(crate) const ASK_USER_MIN_OPTIONS: usize = 2;
pub(crate) const ASK_USER_MAX_OPTIONS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AskUserOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// 推荐选项：用户在超时前未响应时自动选择该项。
    #[serde(default, skip_serializing_if = "is_false")]
    pub recommended: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AskUserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskUserOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AskUserMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AskUserInput {
    pub questions: Vec<AskUserQuestion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AskUserMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingQuestion {
    pub session_id: String,
    pub call_id: String,
    pub questions: Vec<AskUserQuestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AskUserMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_select_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_time: Option<u64>,
}

impl PendingQuestion {
    pub(crate) fn new(
        session_id: String,
        call_id: String,
        input: AskUserInput,
        auto_select_at: Option<u64>,
    ) -> Self {
        Self {
            session_id,
            call_id,
            questions: input.questions,
            metadata: input.metadata,
            auto_select_at,
            server_time: unix_time_millis(),
        }
    }

    pub(crate) fn with_current_server_time(&self) -> Self {
        let mut question = self.clone();
        question.server_time = unix_time_millis();
        question
    }

    /// 每个问题至少有一个推荐选项时返回自动选择的答案；否则 `None`。
    pub(crate) fn auto_recommended_answers(&self) -> Option<HashMap<String, String>> {
        let mut answers = HashMap::new();
        for question in &self.questions {
            let recommended = question
                .options
                .iter()
                .filter(|option| option.recommended)
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>();
            let first = recommended.first()?;
            let answer = if question.multi_select {
                recommended.join(", ")
            } else {
                (*first).to_owned()
            };
            answers.insert(question.question.clone(), answer);
        }
        Some(answers)
    }

    pub(crate) fn validate_answers(&self, answers: &HashMap<String, String>) -> Result<(), String> {
        let expected = self
            .questions
            .iter()
            .map(|question| question.question.as_str())
            .collect::<HashSet<_>>();
        let actual = answers.keys().map(String::as_str).collect::<HashSet<_>>();
        if actual != expected {
            return Err("answers must contain exactly one entry for every question".into());
        }
        if answers.values().any(|answer| answer.trim().is_empty()) {
            return Err("answers must not be empty".into());
        }
        Ok(())
    }
}

fn unix_time_millis() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AnswerRequest {
    pub answers: HashMap<String, String>,
}

pub(crate) fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: ASK_USER_TOOL_NAME.into(),
        description: ("Ask the user one to four multiple-choice questions to clarify \
                       requirements, choose between approaches, or confirm decisions.\n\nPlan \
                       mode: use BEFORE finalizing the plan to gather preferences. Do NOT use to \
                       ask \"is the plan ready?\" — present the plan via upsertSessionPlan, then \
                       confirm exit with askUser or switchMode to code.\n\nUsers can always pick \
                       Other (custom text). Use multiSelect for non-exclusive choices.")
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
                                "description": format!(
                                    "Short chip label (max {ASK_USER_HEADER_MAX_LEN} chars)."
                                )
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
                                        },
                                        "recommended": {
                                            "type": "boolean",
                                            "description": "Mark this option as a recommended default. On timeout, single-select questions use the first marked option; multi-select questions use every marked option."
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
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

pub(crate) fn validate_input(input: &AskUserInput) -> Result<(), String> {
    if input.questions.is_empty() {
        return Err("questions must contain at least one item".into());
    }
    if input.questions.len() > ASK_USER_MAX_QUESTIONS {
        return Err(format!(
            "at most {ASK_USER_MAX_QUESTIONS} questions allowed"
        ));
    }

    let mut seen_questions = HashSet::new();
    for question in &input.questions {
        if question.question.trim().is_empty() {
            return Err("question text must not be empty".into());
        }
        if question.header.trim().is_empty() {
            return Err("question header must not be empty".into());
        }
        if question.header.chars().count() > ASK_USER_HEADER_MAX_LEN {
            return Err(format!(
                "header '{}' exceeds {ASK_USER_HEADER_MAX_LEN} characters",
                question.header
            ));
        }
        if !seen_questions.insert(&question.question) {
            return Err("question texts must be unique".into());
        }
        if question.options.len() < ASK_USER_MIN_OPTIONS
            || question.options.len() > ASK_USER_MAX_OPTIONS
        {
            return Err(format!(
                "question '{}' must have {ASK_USER_MIN_OPTIONS}-{ASK_USER_MAX_OPTIONS} options",
                question.question
            ));
        }
        let mut seen_labels = HashSet::new();
        for option in &question.options {
            if option.label.trim().is_empty() {
                return Err("option labels must not be empty".into());
            }
            if !seen_labels.insert(&option.label) {
                return Err(format!(
                    "option labels must be unique within question '{}'",
                    question.question
                ));
            }
            if question.multi_select && option.preview.is_some() {
                return Err("preview is not supported for multiSelect questions".into());
            }
        }
    }
    Ok(())
}
