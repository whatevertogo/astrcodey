//! Dev-only event snapshots for coarse performance diagnosis.
//!
//! This intentionally samples event boundaries instead of spreading tracing spans
//! through the call graph. A snapshot answers: "what event just happened, and how
//! long has it been since the previous event for the same session/turn?"

#[cfg(debug_assertions)]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Instant,
};

use astrcode_core::event::Event;
#[cfg(debug_assertions)]
use astrcode_core::event::EventPayload;

#[cfg(debug_assertions)]
static LAST_EVENT_AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

#[cfg(debug_assertions)]
pub fn capture_event(source: &'static str, event: &Event) {
    let now = Instant::now();
    let key = snapshot_key(event);
    let since_previous_ms = LAST_EVENT_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut last_event_at| last_event_at.insert(key, now))
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
pub fn capture_event(_source: &'static str, _event: &Event) {}

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
        EventPayload::SessionStarted { .. } => "session_started",
        EventPayload::ModelIdChanged { .. } => "model_id_changed",
        EventPayload::SystemPromptConfigured { .. } => "system_prompt_configured",
        EventPayload::SessionDeleted => "session_deleted",
        EventPayload::AgentSessionSpawned { .. } => "agent_session_spawned",
        EventPayload::AgentRunStarted => "agent_run_started",
        EventPayload::AgentRunCompleted { .. } => "agent_run_completed",
        EventPayload::AgentSessionCompleted { .. } => "agent_session_completed",
        EventPayload::AgentSessionFailed { .. } => "agent_session_failed",
        EventPayload::AgentSessionRecycled { .. } => "agent_session_recycled",
        EventPayload::TurnStarted => "turn_started",
        EventPayload::TurnCompleted { .. } => "turn_completed",
        EventPayload::TurnAbortedContext => "turn_aborted_context",
        EventPayload::UserMessage { .. } => "user_message",
        EventPayload::RecapGenerated { .. } => "recap_generated",
        EventPayload::AssistantMessageStarted { .. } => "assistant_message_started",
        EventPayload::AssistantTextDelta { .. } => "assistant_text_delta",
        EventPayload::AssistantMessageCompleted { .. } => "assistant_message_completed",
        EventPayload::ThinkingDelta { .. } => "thinking_delta",
        EventPayload::ToolCallStarted { .. } => "tool_call_started",
        EventPayload::ToolCallArgumentsDelta { .. } => "tool_call_arguments_delta",
        EventPayload::ToolCallRequested { .. } => "tool_call_requested",
        EventPayload::ToolOutputDelta { .. } => "tool_output_delta",
        EventPayload::ToolCallCompleted { .. } => "tool_call_completed",
        EventPayload::CompactionStarted => "compaction_started",
        EventPayload::CompactionCompleted { .. } => "compaction_completed",
        EventPayload::CompactionSkipped { .. } => "compaction_skipped",
        EventPayload::CompactionFailed { .. } => "compaction_failed",
        EventPayload::CompactBoundaryCreated { .. } => "compact_boundary_created",
        EventPayload::SessionContinuedFromCompaction { .. } => "session_continued_from_compaction",
        EventPayload::SessionForked { .. } => "session_forked",
        EventPayload::ErrorOccurred { .. } => "error_occurred",
        EventPayload::Custom { .. } => "custom",
        EventPayload::ToolCallBackgrounded { .. } => "tool_call_backgrounded",
        EventPayload::BackgroundTaskOutput { .. } => "background_task_output",
        EventPayload::BackgroundTaskNotification { .. } => "background_task_notification",
        EventPayload::BackgroundTaskCompleted { .. } => "background_task_completed",
        EventPayload::ExtensionEvent { .. } => "extension_event",
    }
}

#[cfg(debug_assertions)]
fn payload_details(payload: &EventPayload) -> String {
    match payload {
        EventPayload::SessionStarted {
            model_id,
            parent_session_id,
            source_extension,
            ..
        } => format!(
            "model={model_id} parent={} source={}",
            parent_session_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("-"),
            source_extension.as_deref().unwrap_or("-")
        ),
        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            ..
        } => format!("child={child_session_id} agent={agent_name}"),
        EventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            ..
        } => format!("child={child_session_id} final={final_session_id}"),
        EventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            ..
        } => format!("child={child_session_id} final={final_session_id}"),
        EventPayload::AgentSessionRecycled { child_session_id } => {
            format!("child={child_session_id}")
        },
        EventPayload::ToolCallStarted { call_id, tool_name }
        | EventPayload::ToolCallRequested {
            call_id, tool_name, ..
        }
        | EventPayload::ToolCallCompleted {
            call_id, tool_name, ..
        }
        | EventPayload::ToolCallBackgrounded {
            call_id, tool_name, ..
        }
        | EventPayload::BackgroundTaskCompleted {
            call_id, tool_name, ..
        }
        | EventPayload::BackgroundTaskNotification {
            call_id, tool_name, ..
        } => {
            format!("tool={tool_name} call={call_id}")
        },
        EventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => {
            format!("call={call_id} stream={stream:?} bytes={}", delta.len())
        },
        EventPayload::BackgroundTaskOutput {
            task_id,
            call_id,
            stream,
            delta,
            ..
        } => {
            format!(
                "task={task_id} call={call_id} stream={stream:?} bytes={}",
                delta.len()
            )
        },
        EventPayload::AssistantTextDelta { delta, .. }
        | EventPayload::ThinkingDelta { delta, .. }
        | EventPayload::ToolCallArgumentsDelta { delta, .. } => {
            format!("bytes={}", delta.len())
        },
        EventPayload::UserMessage { text, .. }
        | EventPayload::RecapGenerated { text, .. }
        | EventPayload::AssistantMessageCompleted { text, .. } => {
            format!("bytes={}", text.len())
        },
        EventPayload::TurnCompleted { finish_reason } => format!("reason={finish_reason}"),
        EventPayload::AgentRunCompleted { reason } => format!("reason={reason}"),
        EventPayload::ErrorOccurred {
            code, recoverable, ..
        } => format!("code={code} recoverable={recoverable}"),
        EventPayload::ExtensionEvent {
            extension_id,
            event_type,
            ..
        } => format!("extension={extension_id} event={event_type}"),
        EventPayload::ModelIdChanged { model_id } => format!("model={model_id}"),
        EventPayload::CompactionCompleted { messages_removed } => {
            format!("messages_removed={messages_removed}")
        },
        EventPayload::CompactionSkipped { reason } | EventPayload::CompactionFailed { reason } => {
            format!("reason={reason}")
        },
        EventPayload::CompactBoundaryCreated {
            continued_session_id,
            pre_tokens,
            post_tokens,
            ..
        } => format!(
            "continued={continued_session_id} pre_tokens={pre_tokens} post_tokens={post_tokens}"
        ),
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id, ..
        } => format!("parent={parent_session_id}"),
        EventPayload::SessionForked {
            source_session_id, ..
        } => format!("source={source_session_id}"),
        EventPayload::Custom { name, .. } => format!("name={name}"),
        _ => String::new(),
    }
}
