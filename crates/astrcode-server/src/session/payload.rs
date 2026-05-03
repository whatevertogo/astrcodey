//! Compact continuation 事件载荷构造 helper。
//!
//! 这些 payload 构造函数将 compaction 结果转换为用于父会话边界
//! 事件和子会话继续事件的协议事件负载。

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    types::{Cursor, SessionId},
};

/// 构造父会话 compact continuation 边界事件载荷。
pub(crate) fn compact_boundary_payload(
    trigger: impl Into<String>,
    compaction: &CompactResult,
    continued_session_id: SessionId,
) -> EventPayload {
    EventPayload::CompactBoundaryCreated {
        trigger: trigger.into(),
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        continued_session_id,
    }
}

/// 构造子会话 compact continuation 投影事件载荷。
pub(crate) fn session_continued_from_compaction_payload(
    parent_session_id: SessionId,
    parent_cursor: Cursor,
    compaction: &CompactResult,
) -> EventPayload {
    EventPayload::SessionContinuedFromCompaction {
        parent_session_id,
        parent_cursor,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        context_messages: compaction.context_messages.clone(),
        retained_messages: compaction.retained_messages.clone(),
    }
}
