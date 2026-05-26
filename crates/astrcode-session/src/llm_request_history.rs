//! 从 EventStore 读模型构建 LLM 请求历史（projection SSOT）。

use astrcode_context::prompt_engine::system_messages_from_prompt;
use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::SessionReadModel,
};

use crate::llm_stream::provider_visible_messages;

/// assembler / should_auto_compact 用的「可见历史」（无 system 行）。
pub(crate) fn visible_messages_for_assembler(model: &SessionReadModel) -> Vec<LlmMessage> {
    let mut messages = Vec::with_capacity(
        model
            .context_messages
            .len()
            .saturating_add(model.messages.len()),
    );
    messages.extend(model.context_messages.clone());
    messages.extend(model.messages.clone());
    messages.retain(|message| message.role != LlmRole::System);
    messages
}

/// 组装送 LLM 的完整消息：`system` + `prepare_context_messages` 返回的可见窗口。
///
/// `context_messages` 已是 compact / 未 compact 后的权威可见历史，不再叠加读模型。
pub(crate) fn build_llm_request_messages(
    system_prompt: &str,
    context_messages: Vec<LlmMessage>,
) -> Vec<LlmMessage> {
    let mut messages = Vec::with_capacity(context_messages.len().saturating_add(4));
    messages.extend(system_messages_from_prompt(system_prompt));
    messages.extend(context_messages);
    provider_visible_messages(messages)
}

/// 已提交 tool 结果内容的字符总量（用于 tool 结果预算）。
pub(crate) fn committed_tool_result_content_len(model: &SessionReadModel) -> usize {
    visible_messages_for_assembler(model)
        .iter()
        .filter(|message| message.role == LlmRole::Tool)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            LlmContent::ToolResult { content, .. } => Some(content.len()),
            _ => None,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole},
        storage::SessionReadModel,
        types::new_session_id,
    };

    use super::*;

    fn sample_model() -> SessionReadModel {
        let mut model = SessionReadModel::empty(new_session_id());
        model.messages.push(LlmMessage::user("hello"));
        model
            .messages
            .push(LlmMessage::system("stale system in store"));
        model.context_messages.push(LlmMessage::assistant("ctx"));
        model
    }

    #[test]
    fn visible_messages_excludes_system_and_includes_context() {
        let model = sample_model();
        let visible = visible_messages_for_assembler(&model);
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|m| m.role != LlmRole::System));
        assert!(visible.iter().any(|m| m.role == LlmRole::Assistant));
        assert!(visible.iter().any(|m| m.role == LlmRole::User));
    }

    #[test]
    fn build_llm_request_injects_system_from_prompt() {
        let model = sample_model();
        let messages =
            build_llm_request_messages("fresh system", visible_messages_for_assembler(&model));
        assert!(messages.iter().any(|m| {
            m.role == LlmRole::System
                && m.content.iter().any(|c| {
                    matches!(
                        c,
                        LlmContent::Text { text } if text.contains("fresh")
                    )
                })
        }));
        assert!(!messages.iter().any(|m| {
            m.role == LlmRole::System
                && m.content.iter().any(|c| {
                    matches!(
                        c,
                        LlmContent::Text { text } if text == "stale system in store"
                    )
                })
        }));
    }

    #[test]
    fn committed_tool_result_content_len_sums_tool_messages() {
        let mut model = SessionReadModel::empty(new_session_id());
        model.messages.push(LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "c1".into(),
                content: "abcdef".into(),
                is_error: false,
            }],
            name: Some("tool".into()),
            reasoning_content: None,
        });
        model.messages.push(LlmMessage::user("hi"));
        assert_eq!(committed_tool_result_content_len(&model), 6);
    }
}
