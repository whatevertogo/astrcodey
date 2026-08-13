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
///
/// `source_fingerprint` 是被替换前缀（system prompt + provider 视角消息）的
/// `transcript_prefix_fingerprint`，提交时 projection 重算不匹配则拒绝写入。
pub(crate) fn transcript_rewritten_payload(
    compaction: &CompactResult,
    source_seq: u64,
    source_fingerprint: String,
    strategy: CompactStrategy,
) -> DurableEventPayload {
    let trigger = strategy.trigger().as_str().to_owned();
    let messages = compaction
        .summary_messages
        .iter()
        .chain(&compaction.retained_messages)
        .cloned()
        .collect();
    DurableEventPayload::TranscriptRewritten {
        source_seq,
        source_fingerprint,
        messages,
        reason: TranscriptRewriteReason::Compaction(CompactionDetails {
            trigger,
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
/// `final_session_id` 是应打开/订阅的 leaf（双 id 恒等关系见 `agent_session_final_ids`）。
///
/// [`AgentSessionSpawned`]: astrcode_core::event::DurableEventPayload::AgentSessionSpawned
pub fn agent_session_completed_payload(
    child_session_id: SessionId,
    summary: String,
) -> DurableEventPayload {
    let (child_session_id, final_session_id) = agent_session_final_ids(child_session_id);
    DurableEventPayload::AgentSessionCompleted {
        final_session_id,
        child_session_id,
        summary,
    }
}

/// 构造写入父 session 的 `AgentSessionFailed` 载荷（双 session id 语义见
/// `agent_session_final_ids`）。
pub fn agent_session_failed_payload(
    child_session_id: SessionId,
    error: String,
) -> DurableEventPayload {
    let (child_session_id, final_session_id) = agent_session_final_ids(child_session_id);
    DurableEventPayload::AgentSessionFailed {
        final_session_id,
        child_session_id,
        error,
    }
}

/// 完成/失败载荷共用的双 session id 构造：当前 `final_session_id` 与
/// `child_session_id` 恒等——compact 只重写同一 session 的 transcript，不改变
/// session id。若未来落地跨 session continuation 使二者不同，只需改这里。
fn agent_session_final_ids(child_session_id: SessionId) -> (SessionId, SessionId) {
    let final_session_id = child_session_id.clone();
    (child_session_id, final_session_id)
}

#[cfg(test)]
mod tests {
    use astrcode_context::CompactResult;
    use astrcode_core::{
        compaction::CompactStrategy,
        event::{DurableEventPayload, TranscriptRewriteReason},
        llm::LlmMessage,
    };

    use super::transcript_rewritten_payload;

    #[test]
    fn transcript_rewrite_contains_compacted_history_and_strategy_metadata() {
        let compaction = CompactResult {
            pre_tokens: 100,
            post_tokens: 20,
            summary: "summary".into(),
            messages_removed: 2,
            summary_messages: vec![LlmMessage::user("summary context")],
            retained_messages: vec![LlmMessage::user("retained")],
            transcript_path: Some("compact.jsonl".into()),
        };

        let rewrite = transcript_rewritten_payload(
            &compaction,
            7,
            "fingerprint".to_owned(),
            CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        );

        assert!(matches!(
            rewrite,
            DurableEventPayload::TranscriptRewritten {
                source_seq: 7,
                source_fingerprint,
                messages,
                reason: TranscriptRewriteReason::Compaction(details),
            } if source_fingerprint == "fingerprint"
                && messages.len() == 2
                && details.trigger == "manual_command"
                && details.transcript_path.as_deref() == Some("compact.jsonl")
        ));
    }
}
