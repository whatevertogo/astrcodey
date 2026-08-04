//! Dev-only session event snapshots for coarse performance diagnosis.
//!
//! This intentionally samples event boundaries instead of spreading tracing spans
//! through the call graph. A snapshot answers: "what event just happened, and how
//! long has it been since the previous event for the same session/turn?"
//!
//! 新增事件变体时必须同步更新 `payload_type` 与 `payload_details` 两组 match
//! （及其各自的 durable_*/live_* 分支），否则 dev snapshot 会缺失类型或详情。

#[cfg(debug_assertions)]
use std::{collections::HashMap, sync::OnceLock, time::Instant};

use astrcode_core::event::Event;
#[cfg(debug_assertions)]
use astrcode_core::event::{
    DurableEventPayload, EventPayload, LiveEventPayload, TranscriptRewriteReason,
};
#[cfg(debug_assertions)]
use parking_lot::Mutex;

#[cfg(debug_assertions)]
static LAST_EVENT_AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

#[cfg(debug_assertions)]
pub(crate) fn capture_event(source: &'static str, event: &Event) {
    let now = Instant::now();
    let key = snapshot_key(event);
    let since_previous_ms = LAST_EVENT_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .insert(key, now)
        .map(|previous| now.duration_since(previous).as_millis());

    tracing::debug!(
        target: "astrcode::dev_snapshot",
        source,
        session_id = %event.session_id,
        turn_id = event.turn_id.as_ref().map(|id| id.as_str()).unwrap_or("-"),
        event_type = payload_type(&event.payload),
        details = payload_details(&event.payload),
        seq = event.seq,
        since_previous_ms,
        "dev event snapshot"
    );
}

#[cfg(not(debug_assertions))]
pub(crate) fn capture_event(_source: &'static str, _event: &Event) {}

#[cfg(debug_assertions)]
fn snapshot_key(event: &Event) -> String {
    match &event.turn_id {
        Some(turn_id) => format!("{}:{turn_id}", event.session_id),
        None => event.session_id.to_string(),
    }
}

#[cfg(debug_assertions)]
fn payload_type(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::Durable(payload) => durable_payload_type(payload),
        EventPayload::Live(payload) => live_payload_type(payload),
    }
}

#[cfg(debug_assertions)]
fn durable_payload_type(payload: &DurableEventPayload) -> &'static str {
    match payload {
        DurableEventPayload::SessionStarted(_) => "session_started",
        DurableEventPayload::ModelIdChanged { .. } => "model_id_changed",
        DurableEventPayload::SessionToolsConfigured { .. } => "session_tools_configured",
        DurableEventPayload::SystemPromptConfigured { .. } => "system_prompt_configured",
        DurableEventPayload::AgentSessionSpawned { .. } => "agent_session_spawned",
        DurableEventPayload::AgentSessionCompleted { .. } => "agent_session_completed",
        DurableEventPayload::AgentSessionFailed { .. } => "agent_session_failed",
        DurableEventPayload::AgentSessionRecycled { .. } => "agent_session_recycled",
        DurableEventPayload::TurnStarted => "turn_started",
        DurableEventPayload::TurnCompleted { .. } => "turn_completed",
        DurableEventPayload::TurnAbortedContext => "turn_aborted_context",
        DurableEventPayload::UserInputAccepted { .. } => "user_input_accepted",
        DurableEventPayload::UserMessage { .. } => "user_message",
        DurableEventPayload::RecapGenerated { .. } => "recap_generated",
        DurableEventPayload::AssistantMessageCompleted { .. } => "assistant_message_completed",
        DurableEventPayload::TokenUsageRecorded { .. } => "token_usage_recorded",
        DurableEventPayload::ToolCallRequested { .. } => "tool_call_requested",
        DurableEventPayload::ToolApprovalRequested { .. } => "tool_approval_requested",
        DurableEventPayload::ToolApprovalResolved { .. } => "tool_approval_resolved",
        DurableEventPayload::ToolCallCompleted { .. } => "tool_call_completed",
        DurableEventPayload::ToolCallFailed { .. } => "tool_call_failed",
        DurableEventPayload::ToolCallCancelled { .. } => "tool_call_cancelled",
        DurableEventPayload::TranscriptRewritten { .. } => "transcript_rewritten",
        DurableEventPayload::SessionForked { .. } => "session_forked",
        DurableEventPayload::ErrorOccurred { .. } => "error_occurred",
        DurableEventPayload::ExtensionEvent(_) => "extension_event",
    }
}

#[cfg(debug_assertions)]
fn live_payload_type(payload: &LiveEventPayload) -> &'static str {
    match payload {
        LiveEventPayload::AgentRunStarted => "agent_run_started",
        LiveEventPayload::AgentRunCompleted { .. } => "agent_run_completed",
        LiveEventPayload::LlmRetrying { .. } => "llm_retrying",
        LiveEventPayload::LlmRetryRecovered => "llm_retry_recovered",
        LiveEventPayload::AssistantMessageStarted { .. } => "assistant_message_started",
        LiveEventPayload::AssistantMessageReset { .. } => "assistant_message_reset",
        LiveEventPayload::AssistantTextDelta { .. } => "assistant_text_delta",
        LiveEventPayload::ThinkingDelta { .. } => "thinking_delta",
        LiveEventPayload::ToolCallStarted { .. } => "tool_call_started",
        LiveEventPayload::ToolCallArgumentsDelta { .. } => "tool_call_arguments_delta",
        LiveEventPayload::ToolOutputDelta { .. } => "tool_output_delta",
        LiveEventPayload::CompactionStarted => "compaction_started",
        LiveEventPayload::CompactionCompleted { .. } => "compaction_completed",
        LiveEventPayload::CompactionSkipped { .. } => "compaction_skipped",
        LiveEventPayload::CompactionFailed { .. } => "compaction_failed",
        LiveEventPayload::ErrorOccurred { .. } => "error_occurred",
        LiveEventPayload::ExtensionEvent(_) => "extension_event",
    }
}

#[cfg(debug_assertions)]
fn payload_details(payload: &EventPayload) -> String {
    match payload {
        EventPayload::Durable(payload) => durable_payload_details(payload),
        EventPayload::Live(payload) => live_payload_details(payload),
    }
}

#[cfg(debug_assertions)]
fn durable_payload_details(payload: &DurableEventPayload) -> String {
    match payload {
        DurableEventPayload::SessionStarted(started) => format!(
            "model={} parent={} source={}",
            started.model_id,
            started
                .parent
                .as_ref()
                .map(|parent| parent.session_id.as_str())
                .unwrap_or("-"),
            started.source_extension.as_deref().unwrap_or("-")
        ),
        DurableEventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            ..
        } => format!("child={child_session_id} agent={agent_name}"),
        DurableEventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            ..
        } => format!("child={child_session_id} final={final_session_id}"),
        DurableEventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            ..
        } => format!("child={child_session_id} final={final_session_id}"),
        DurableEventPayload::AgentSessionRecycled { child_session_id } => {
            format!("child={child_session_id}")
        },
        DurableEventPayload::ToolCallRequested {
            call_id, tool_name, ..
        }
        | DurableEventPayload::ToolCallCompleted {
            call_id, tool_name, ..
        }
        | DurableEventPayload::ToolCallFailed {
            call_id, tool_name, ..
        }
        | DurableEventPayload::ToolCallCancelled {
            call_id, tool_name, ..
        } => {
            format!("tool={tool_name} call={call_id}")
        },
        DurableEventPayload::UserMessage { text, .. }
        | DurableEventPayload::RecapGenerated { text, .. }
        | DurableEventPayload::AssistantMessageCompleted { text, .. } => {
            format!("bytes={}", text.len())
        },
        DurableEventPayload::TurnCompleted { finish_reason } => {
            format!("reason={finish_reason}")
        },
        DurableEventPayload::TokenUsageRecorded {
            usage,
            model_context_window,
        } => format!(
            "input={:?} cached={:?} output={:?} reasoning={:?} total={:?} context_window={}",
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
            usage.total_tokens,
            model_context_window
        ),
        DurableEventPayload::ErrorOccurred {
            code, recoverable, ..
        } => format!("code={code} recoverable={recoverable}"),
        DurableEventPayload::ExtensionEvent(event) => {
            format!(
                "extension={} event={}",
                event.extension_id, event.event_type
            )
        },
        DurableEventPayload::ModelIdChanged { model_id } => format!("model={model_id}"),
        DurableEventPayload::TranscriptRewritten {
            source_seq,
            reason: TranscriptRewriteReason::Compaction(details),
            ..
        } => format!(
            "source_seq={source_seq} pre_tokens={} post_tokens={}",
            details.pre_tokens, details.post_tokens
        ),
        DurableEventPayload::SessionForked {
            source_session_id, ..
        } => format!("source={source_session_id}"),
        _ => String::new(),
    }
}

#[cfg(debug_assertions)]
fn live_payload_details(payload: &LiveEventPayload) -> String {
    match payload {
        LiveEventPayload::ToolCallStarted { call_id, tool_name } => {
            format!("tool={tool_name} call={call_id}")
        },
        LiveEventPayload::LlmRetrying {
            status,
            attempt,
            max_retries,
            delay_ms,
        } => format!(
            "status={} attempt={attempt}/{max_retries} delay_ms={delay_ms}",
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "transport".into())
        ),
        LiveEventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => format!("call={call_id} stream={stream:?} bytes={}", delta.len()),
        LiveEventPayload::AssistantTextDelta { delta, .. }
        | LiveEventPayload::ThinkingDelta { delta, .. }
        | LiveEventPayload::ToolCallArgumentsDelta { delta, .. } => {
            format!("bytes={}", delta.len())
        },
        LiveEventPayload::AgentRunCompleted { reason }
        | LiveEventPayload::CompactionSkipped { reason }
        | LiveEventPayload::CompactionFailed { reason } => format!("reason={reason}"),
        LiveEventPayload::ErrorOccurred {
            code, recoverable, ..
        } => format!("code={code} recoverable={recoverable}"),
        LiveEventPayload::CompactionCompleted { messages_removed } => {
            format!("messages_removed={messages_removed}")
        },
        LiveEventPayload::ExtensionEvent(event) => {
            format!(
                "extension={} event={}",
                event.extension_id, event.event_type
            )
        },
        _ => String::new(),
    }
}
