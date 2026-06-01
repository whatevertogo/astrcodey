//! Mid-turn 用户输入在 agent step 边界的同步检测。
//!
//! 消息由 `TurnScheduler::inject_internal` 立即写入 durable `UserMessage`（无内存 buffer）。
//! 本模块在每个 step 开始前统计读模型中的 user 条数；增量写入
//! [`LifecycleContext::mid_turn_user_messages_synced`] 并随 [`ExtensionEvent::StepStart`] 派发。

use astrcode_context::compaction::is_synthetic_context_message;
use astrcode_core::{llm::LlmRole, storage::SessionReadModel};

/// 统计读模型中 provider 可见的非合成 user 消息条数。
pub(crate) fn count_visible_user_messages(model: &SessionReadModel) -> usize {
    model
        .messages
        .iter()
        .filter(|entry| {
            entry.message.role == LlmRole::User && !is_synthetic_context_message(&entry.message)
        })
        .count()
}

/// 是否存在尚未并入 LLM 上下文的 mid-turn user 消息（如后台 shell 完成通知）。
pub(crate) fn has_pending_mid_turn_user_messages(
    model: &SessionReadModel,
    tracked_count: usize,
) -> bool {
    count_visible_user_messages(model) > tracked_count
}

#[cfg(test)]
mod tests {
    use astrcode_core::{llm::LlmMessage, storage::SequencedLlmMessage, types::SessionId};

    use super::*;

    fn model_with_messages(messages: Vec<LlmMessage>) -> SessionReadModel {
        let mut model = SessionReadModel::empty(SessionId::new("s-test"));
        model.messages = messages
            .into_iter()
            .enumerate()
            .map(|(updated_seq, message)| SequencedLlmMessage {
                message,
                updated_seq: updated_seq as u64,
                source: None,
            })
            .collect();
        model
    }

    #[test]
    fn count_visible_user_messages_excludes_compact_summary_marker() {
        let model = model_with_messages(vec![
            LlmMessage::user("real"),
            LlmMessage::user("<compact_summary>summary</compact_summary>"),
            LlmMessage::user("also real"),
        ]);
        assert_eq!(count_visible_user_messages(&model), 2);
    }

    #[test]
    fn has_pending_mid_turn_user_messages_detects_unsynced_inject() {
        let model = model_with_messages(vec![
            LlmMessage::user("hello"),
            LlmMessage::user("<background-shell-notification>done</background-shell-notification>"),
        ]);
        assert!(!has_pending_mid_turn_user_messages(&model, 2));
        assert!(has_pending_mid_turn_user_messages(&model, 1));
    }
}
