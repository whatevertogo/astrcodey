//! 事件载荷构造。

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    extension::CompactStrategy,
    types::{Cursor, SessionId},
};

/// 构造 session 当前 system prompt 配置的持久事件载荷。
pub fn system_prompt_configured_payload(
    text: String,
    fingerprint: String,
    extra_system_prompt: Option<String>,
) -> EventPayload {
    EventPayload::SystemPromptConfigured {
        text,
        fingerprint,
        extra_system_prompt,
    }
}

/// 构造 compact continuation 边界事件载荷。
pub fn compact_boundary_payload(
    trigger: impl Into<String>,
    compaction: &CompactResult,
    continued_session_id: SessionId,
    base_event_seq: u64,
    strategy: CompactStrategy,
) -> EventPayload {
    EventPayload::CompactBoundaryCreated {
        trigger: trigger.into(),
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        continued_session_id,
        base_event_seq,
        strategy,
    }
}

/// 构造子会话 compact continuation 投影事件载荷。
pub fn session_continued_from_compaction_payload(
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
