//! Compact 事件载荷构造 helper。
//!
//! 将 `CompactResult` 拆分为投影事件（`CompactionApplied`）和审计事件
//! （`CompactionCompleted`），两者各司其职、不重复字段。

use astrcode_context::compaction::CompactResult;
use astrcode_core::event::EventPayload;

/// 构造 compact 投影事件载荷。
pub(crate) fn compaction_applied_payload(compaction: &CompactResult) -> EventPayload {
    EventPayload::CompactionApplied {
        messages_removed: compaction.messages_removed,
        context_messages: compaction.context_messages.clone(),
    }
}

/// 构造 compact 完成事件载荷。
pub(crate) fn compaction_completed_payload(compaction: &CompactResult) -> EventPayload {
    EventPayload::CompactionCompleted {
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
    }
}
