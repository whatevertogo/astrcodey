//! 轻量消息类型，用于扩展 API 边界。
//!
//! 扩展通过 `ProviderResult` 和 `ProviderContext` 与宿主交互时使用这些类型，
//! 而非直接使用 `LlmMessage`/`LlmRole`——后者包含 provider 专用细节
//! （如 `reasoning_content`、`ToolCall` content 等），不应暴露给扩展。
//!
//! # 与 LlmMessage 的关系
//!
//! `PromptMessage` 是 `LlmMessage` 的子集。宿主侧负责双向转换：
//! - `ProviderContext.messages`：宿主将 `LlmMessage` 转换为 `PromptMessage` 传给扩展
//! - `ProviderResult::AppendMessages`：扩展返回 `PromptMessage`，宿主转换为 `LlmMessage`

use serde::{Deserialize, Serialize};

// ─── PromptRole ────────────────────────────────────────────────────────

/// 扩展可见的消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRole {
    System,
    User,
    Assistant,
}

// ─── PromptMessage ─────────────────────────────────────────────────────

/// 扩展可见的消息。只包含文本内容，不含 provider 专用细节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub text: String,
}

impl PromptMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: PromptRole::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: PromptRole::Assistant,
            text: text.into(),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: PromptRole::System,
            text: text.into(),
        }
    }
}

// ─── Conversion: LlmMessage ↔ PromptMessage ───────────────────────────

impl From<&crate::llm::LlmMessage> for PromptMessage {
    fn from(msg: &crate::llm::LlmMessage) -> Self {
        use crate::llm::{LlmContent, LlmRole};
        let role = match msg.role {
            LlmRole::System => PromptRole::System,
            LlmRole::User => PromptRole::User,
            LlmRole::Assistant => PromptRole::Assistant,
            LlmRole::Tool => PromptRole::Assistant, // tool messages 归入 assistant 侧
        };
        // 提取文本内容：拼接所有 Text 块
        let text: String = msg
            .content
            .iter()
            .filter_map(|c| match c {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self { role, text }
    }
}

impl From<PromptMessage> for crate::llm::LlmMessage {
    fn from(msg: PromptMessage) -> Self {
        use crate::llm::{LlmContent, LlmMessage, LlmRole};
        let role = match msg.role {
            PromptRole::System => LlmRole::System,
            PromptRole::User => LlmRole::User,
            PromptRole::Assistant => LlmRole::Assistant,
        };
        LlmMessage {
            role,
            content: vec![LlmContent::Text { text: msg.text }],
            name: None,
            reasoning_content: None,
        }
    }
}

// ─── ModelInfo ─────────────────────────────────────────────────────────

/// 扩展可见的模型信息。`ModelSelection` 的轻量替代。
///
/// 扩展只需要知道正在使用哪个模型，不需要 `profile_name` 或 `provider_kind`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// 模型标识符（如 "claude-sonnet-4-20250514"）。
    pub model: String,
}

impl ModelInfo {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

impl From<&crate::config::ModelSelection> for ModelInfo {
    fn from(ms: &crate::config::ModelSelection) -> Self {
        Self {
            model: ms.model.clone(),
        }
    }
}

// ─── Session View Types ───────────────────────────────────────────────

/// 扩展可见的会话摘要。`SessionSummary` 的轻量替代。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummaryView {
    pub session_id: String,
    pub working_dir: String,
    pub model: String,
    pub first_user_message: Option<String>,
    /// 父会话 ID。用于过滤子会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 创建该会话的扩展 ID。用于过滤扩展创建的会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_extension: Option<String>,
    /// 最后更新时间（RFC 3339 格式）。用于判断会话是否空闲。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// 扩展可见的对话视图。`SessionReadModel` 的轻量替代。
///
/// 宿主侧在 `EventQueryCap.read_conversation()` 实现中负责
/// 将 `SessionReadModel` 转换为此类型，过滤掉 provider 专用细节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationView {
    pub session_id: String,
    pub turns: Vec<TurnView>,
}

/// 单轮对话的视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnView {
    pub role: PromptRole,
    pub text: String,
}

impl From<&PromptMessage> for TurnView {
    fn from(msg: &PromptMessage) -> Self {
        Self {
            role: msg.role,
            text: msg.text.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_message_user() {
        let msg = PromptMessage::user("hello");
        assert_eq!(msg.role, PromptRole::User);
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn prompt_message_system() {
        let msg = PromptMessage::system("you are helpful");
        assert_eq!(msg.role, PromptRole::System);
    }

    #[test]
    fn llm_message_round_trip() {
        let llm = crate::llm::LlmMessage::user("hello");
        let prompt: PromptMessage = (&llm).into();
        assert_eq!(prompt.role, PromptRole::User);
        assert_eq!(prompt.text, "hello");

        let back: crate::llm::LlmMessage = prompt.into();
        assert_eq!(back.role, crate::llm::LlmRole::User);
        assert_eq!(
            back.content,
            vec![crate::llm::LlmContent::Text {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn model_info_from_selection() {
        let ms = crate::config::ModelSelection::simple("opus");
        let info = ModelInfo::from(&ms);
        assert_eq!(info.model, "opus");
    }
}
