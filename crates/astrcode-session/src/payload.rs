//! 事件载荷构造。

use astrcode_context::CompactResult;
use astrcode_core::{
    compaction::CompactStrategy,
    event::{
        CompactionDetails, DurableEventPayload, LiveEventPayload, SystemPromptSource,
        TranscriptRewriteReason,
    },
    types::SessionId,
};

pub const TURN_FINISH_ABORTED: &str = "aborted";
pub const TURN_FINISH_INTERRUPTED: &str = "interrupted";

pub fn turn_completed_payload(reason: impl Into<String>) -> DurableEventPayload {
    DurableEventPayload::TurnCompleted {
        finish_reason: reason.into(),
    }
}

pub fn agent_run_completed_payload(reason: impl Into<String>) -> LiveEventPayload {
    LiveEventPayload::AgentRunCompleted {
        reason: reason.into(),
    }
}

/// 构造 session 当前 system prompt 配置的持久事件载荷。
pub fn system_prompt_configured_payload(
    text: String,
    fingerprint: String,
    extra_system_prompt: Option<String>,
    source: SystemPromptSource,
) -> DurableEventPayload {
    DurableEventPayload::SystemPromptConfigured {
        text,
        fingerprint,
        extra_system_prompt,
        source,
    }
}

/// 构造原子的 transcript 重写事件。
pub fn transcript_rewritten_payload(
    trigger: impl Into<String>,
    compaction: &CompactResult,
    source_seq: u64,
    strategy: CompactStrategy,
) -> DurableEventPayload {
    let messages = compaction
        .summary_messages
        .iter()
        .chain(&compaction.retained_messages)
        .cloned()
        .collect();
    DurableEventPayload::TranscriptRewritten {
        source_seq,
        messages,
        reason: TranscriptRewriteReason::Compaction(CompactionDetails {
            trigger: trigger.into(),
            pre_tokens: compaction.pre_tokens,
            post_tokens: compaction.post_tokens,
            summary: compaction.summary.clone(),
            transcript_path: compaction.transcript_path.clone(),
            strategy,
        }),
    }
}

/// 子 agent 终态事件里 `child_session_id` / `final_session_id` 的唯一构造点。
///
/// **给后续维护者**：compact 只重写同一 session 的 transcript，不改变 session id。
///
/// - `child_session_id`：与父 log 中 [`AgentSessionSpawned`] 一致，投影靠它定位 link。
/// - `final_session_id`：应打开/订阅的 leaf；**仅**在未实现的跨 session continuation
///   落地后才可能与前者不同。勿手写双字段，统一走 [`agent_session_completed_payload`] /
///   [`agent_session_failed_payload`]。
///
/// [`AgentSessionSpawned`]: astrcode_core::event::DurableEventPayload::AgentSessionSpawned
fn agent_session_terminal_ids(child_session_id: SessionId) -> (SessionId, SessionId) {
    let final_session_id = child_session_id.clone();
    (child_session_id, final_session_id)
}

/// 构造写入父 session 的 `AgentSessionCompleted` 载荷（双 session id 见
/// [`agent_session_terminal_ids`]）。
pub fn agent_session_completed_payload(
    child_session_id: SessionId,
    summary: String,
) -> DurableEventPayload {
    let (child_session_id, final_session_id) = agent_session_terminal_ids(child_session_id);
    DurableEventPayload::AgentSessionCompleted {
        child_session_id,
        final_session_id,
        summary,
    }
}

/// 构造写入父 session 的 `AgentSessionFailed` 载荷（双 session id 见
/// [`agent_session_terminal_ids`]）。
pub fn agent_session_failed_payload(
    child_session_id: SessionId,
    error: String,
) -> DurableEventPayload {
    let (child_session_id, final_session_id) = agent_session_terminal_ids(child_session_id);
    DurableEventPayload::AgentSessionFailed {
        child_session_id,
        final_session_id,
        error,
    }
}
