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

/// 子 agent 终态事件里 `child_session_id` / `final_session_id` 的唯一构造点。
///
/// **给后续维护者**：协议保留两个字段，但当前实现里 compact **不会**换
/// `session_id`（见 `Session::append_compact_boundary`：boundary 与
/// `SessionContinuedFromCompaction` 都写在同一条 log 上，`continued_session_id`、
/// `parent_session_id` 均为 `self.id`）。因此 compact 前后、以及
/// `AgentSessionCompleted` 写入时，两者应相同。
///
/// - `child_session_id`：与父 log 中 [`AgentSessionSpawned`] 一致，投影靠它定位 link。
/// - `final_session_id`：应打开/订阅的 leaf；**仅**在未实现的跨 session continuation
///   落地后才可能与前者不同。勿手写双字段，统一走 [`agent_session_completed_payload`] /
///   [`agent_session_failed_payload`]。
///
/// [`AgentSessionSpawned`]: astrcode_core::event::EventPayload::AgentSessionSpawned
fn agent_session_terminal_ids(child_session_id: SessionId) -> (SessionId, SessionId) {
    let final_session_id = child_session_id.clone();
    (child_session_id, final_session_id)
}

/// 构造写入父 session 的 `AgentSessionCompleted` 载荷（双 session id 见
/// [`agent_session_terminal_ids`]）。
pub fn agent_session_completed_payload(
    child_session_id: SessionId,
    summary: String,
) -> EventPayload {
    let (child_session_id, final_session_id) = agent_session_terminal_ids(child_session_id);
    EventPayload::AgentSessionCompleted {
        child_session_id,
        final_session_id,
        summary,
    }
}

/// 构造写入父 session 的 `AgentSessionFailed` 载荷（双 session id 见
/// [`agent_session_terminal_ids`]）。
pub fn agent_session_failed_payload(child_session_id: SessionId, error: String) -> EventPayload {
    let (child_session_id, final_session_id) = agent_session_terminal_ids(child_session_id);
    EventPayload::AgentSessionFailed {
        child_session_id,
        final_session_id,
        error,
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
