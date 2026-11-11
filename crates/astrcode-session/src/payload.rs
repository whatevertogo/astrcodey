//! 事件载荷构造。

use astrcode_context::CompactResult;
use astrcode_core::{
    compaction::CompactStrategy,
    event::{
        AgentSessionOutcome, CompactionDetails, DurableEventPayload, LiveEventPayload,
        PersistedSystemPrompt, SystemPromptSource, TranscriptRewriteReason, TurnEndReason,
    },
    llm::TranscriptMessage,
    types::SessionId,
};

/// JSON-RPC 内部错误码，用于 ErrorOccurred 载荷。
pub const JSON_RPC_INTERNAL_ERROR: i32 = -32603;

pub fn turn_ended_payload(reason: TurnEndReason) -> DurableEventPayload {
    DurableEventPayload::TurnEnded { reason }
}

pub fn agent_run_completed_payload(reason: &TurnEndReason) -> LiveEventPayload {
    LiveEventPayload::AgentRunEnded {
        reason: reason.clone(),
    }
}

/// 构造 session 当前 system prompt 配置的持久事件载荷。
pub fn system_prompt_configured_payload(
    text: String,
    fingerprint: String,
    extra_system_prompt: Option<String>,
    source: SystemPromptSource,
) -> DurableEventPayload {
    DurableEventPayload::SystemPromptConfigured(PersistedSystemPrompt {
        text,
        fingerprint,
        extra_system_prompt,
        source,
    })
}

/// 构造 transcript 前缀重写事件；`source_seq` 之后的 tail 由 projection 保留。
///
/// `source_fingerprint` 是被替换前缀（system prompt + provider 视角消息）的
/// `transcript_prefix_fingerprint`，提交时 projection 重算不匹配则拒绝写入。
pub(crate) fn transcript_rewritten_payload(
    compaction: &CompactResult,
    retained_messages: &[TranscriptMessage],
    source_seq: u64,
    source_fingerprint: String,
    strategy: CompactStrategy,
) -> DurableEventPayload {
    let trigger = strategy.trigger().as_str().to_owned();
    let messages = compaction
        .summary_messages
        .iter()
        .cloned()
        .map(TranscriptMessage::plain)
        .chain(retained_messages.iter().cloned())
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

/// 构造写入父 session 的 `AgentSessionSettled` 成功载荷。
///
/// `child_session_id` 与父 log 中 [`AgentSessionSpawned`] 一致，投影靠它定位 link；
///
/// [`AgentSessionSpawned`]: astrcode_core::event::DurableEventPayload::AgentSessionSpawned
pub fn agent_session_completed_payload(
    child_session_id: SessionId,
    summary: String,
) -> DurableEventPayload {
    DurableEventPayload::AgentSessionSettled {
        child_session_id,
        outcome: AgentSessionOutcome::Completed { summary },
    }
}

/// 构造写入父 session 的 `AgentSessionSettled` 失败载荷。
pub fn agent_session_failed_payload(
    child_session_id: SessionId,
    error: String,
) -> DurableEventPayload {
    DurableEventPayload::AgentSessionSettled {
        child_session_id,
        outcome: AgentSessionOutcome::Failed { error },
    }
}

#[cfg(test)]
mod tests {
    use astrcode_context::CompactResult;
    use astrcode_core::{
        compaction::CompactStrategy,
        event::{DurableEventPayload, TranscriptRewriteReason},
        llm::{LlmMessage, TranscriptMessage, TranscriptMessageOrigin},
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
            &[TranscriptMessage {
                message: LlmMessage::user("retained"),
                origin: Some(TranscriptMessageOrigin::TurnAborted),
            }],
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
                && messages[0].origin.is_none()
                && messages[1].origin == Some(TranscriptMessageOrigin::TurnAborted)
                && details.trigger == "manual_command"
                && details.transcript_path.as_deref() == Some("compact.jsonl")
        ));
    }
}
