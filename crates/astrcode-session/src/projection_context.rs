//! 从 durable projection 派生运行时 context。

use astrcode_context::ContextSnapshot;
use astrcode_core::llm::{LlmContent, LlmRole};
use astrcode_session_projection::SessionReadModel;

pub(crate) fn context_snapshot(model: &SessionReadModel) -> ContextSnapshot {
    let Some(usage) = &model.context_usage else {
        return ContextSnapshot::new(
            model.stats.last_seq,
            model.system_prompt.text.clone(),
            model
                .transcript
                .messages
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
        );
    };
    // covered 前缀必须再克隆一份传给 anchor：with_input_token_anchor 只接收 owned
    // Vec 并在过滤空间重算 covered 计数（provider_visible_messages 含相邻 assistant
    // 合并等非逐条变换，无法从已过滤的 snapshot.messages 反推），所以前缀消息会
    // 随全量克隆各出现一次——这是 API 形状决定的必要克隆，不是冗余。
    let covered_messages = model
        .transcript
        .messages
        .iter()
        .take(usage.covered_message_count)
        .map(|entry| entry.message.clone())
        .collect();
    let snapshot = ContextSnapshot::new(
        model.stats.last_seq,
        model.system_prompt.text.clone(),
        model
            .transcript
            .messages
            .iter()
            .map(|entry| entry.message.clone())
            .collect(),
    );
    snapshot.with_input_token_anchor(
        usage.context_tokens,
        usage.model_context_window,
        covered_messages,
    )
}

/// 已提交 tool 结果内容的字符总量（用于 tool 结果预算）。
pub(crate) fn committed_tool_result_content_len(model: &SessionReadModel) -> usize {
    model
        .transcript
        .messages
        .iter()
        .map(|entry| &entry.message)
        .filter(|message| message.role == LlmRole::Tool)
        .flat_map(|message| message.content.iter())
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
        types::new_session_id,
    };
    use astrcode_session_projection::{SequencedLlmMessage, SessionReadModel};

    use super::*;
    use crate::test_support::read_model;

    fn sample_model() -> SessionReadModel {
        let mut model = read_model(new_session_id());
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage::user("hello"),
            updated_seq: 1,
            source: None,
        });
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage::system("stale system in store"),
            updated_seq: 2,
            source: None,
        });
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage::assistant("ctx"),
            updated_seq: 3,
            source: None,
        });
        model
    }

    #[test]
    fn context_snapshot_excludes_system_and_includes_transcript() {
        let model = sample_model();
        let snapshot = context_snapshot(&model);
        assert_eq!(snapshot.messages.len(), 2);
        assert!(
            snapshot
                .messages
                .iter()
                .all(|message| message.role != LlmRole::System)
        );
    }

    #[test]
    fn context_snapshot_builds_request_with_its_system_prompt() {
        let mut model = sample_model();
        model.system_prompt.text = "fresh system".into();
        let snapshot = context_snapshot(&model);
        let messages = snapshot.request_messages(snapshot.messages.clone());
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
        let mut model = sample_model();
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage {
                role: LlmRole::Tool,
                content: vec![LlmContent::ToolResult {
                    tool_call_id: "c1".into(),
                    content: "abcdef".into(),
                    is_error: false,
                }],
                name: Some("tool".into()),
                reasoning_content: None,
            },
            updated_seq: 1,
            source: None,
        });
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage::user("hi"),
            updated_seq: 2,
            source: None,
        });
        assert_eq!(committed_tool_result_content_len(&model), 6);
    }
}
