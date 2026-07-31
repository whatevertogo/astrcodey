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
pub const TURN_FINISH_ERROR: &str = "error";
/// JSON-RPC 内部错误码，用于 ErrorOccurred 载荷。
pub const JSON_RPC_INTERNAL_ERROR: i32 = -32603;

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

/// 构造 transcript 前缀重写事件；`source_seq` 之后的 tail 由 projection 保留。
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

/// 构造写入父 session 的 `AgentSessionCompleted` 载荷。
///
/// `child_session_id` 与父 log 中 [`AgentSessionSpawned`] 一致，投影靠它定位 link；
/// `final_session_id` 是应打开/订阅的 leaf。当前两者恒等——compact 只重写同一 session
/// 的 transcript，不改变 session id。若未来落地跨 session continuation 使二者不同，
/// 再在此处引入区分逻辑。
///
/// [`AgentSessionSpawned`]: astrcode_core::event::DurableEventPayload::AgentSessionSpawned
pub fn agent_session_completed_payload(
    child_session_id: SessionId,
    summary: String,
) -> DurableEventPayload {
    DurableEventPayload::AgentSessionCompleted {
        final_session_id: child_session_id.clone(),
        child_session_id,
        summary,
    }
}

/// 构造写入父 session 的 `AgentSessionFailed` 载荷（双 session id 语义见
/// [`agent_session_completed_payload`]）。
pub fn agent_session_failed_payload(
    child_session_id: SessionId,
    error: String,
) -> DurableEventPayload {
    DurableEventPayload::AgentSessionFailed {
        final_session_id: child_session_id.clone(),
        child_session_id,
        error,
    }
}
