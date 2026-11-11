//! Route child-session events to the owning tool-call tracker.

use astrcode_core::event::{
    DurableEventPayload, Event, EventPayload, LiveEventPayload, ToolCallOutcome,
};

use super::{
    App,
    tool_summary::{CHILD_SUMMARY_FORMAT, tool_completion_summary, truncate_first_line},
};
use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

pub(super) fn is_tracked_child(app: &App, child_session_id: &str) -> bool {
    app.child_session_map
        .get(child_session_id)
        .is_some_and(|call_id| app.child_agents.contains_key(call_id))
}

/// 处理来自子 session 的事件，将工具调用进度路由到对应的 ChildAgentTracker。
pub(super) fn apply_child_session_event(app: &mut App, call_id: &str, event: &Event) {
    match &event.payload {
        EventPayload::Durable(DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            ..
        }) => {
            app.tool_names
                .insert(call_id.to_string(), tool_name.clone());
        },
        EventPayload::Live(LiveEventPayload::ToolExecutionStarted { tool_name, .. }) => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                tracker.on_tool_started(tool_name);
                app.status_text = format!("● Task → {tool_name}");
            }
        },
        EventPayload::Durable(DurableEventPayload::ToolCallSettled {
            call_id: tool_call_id,
            result,
            outcome,
        }) => {
            let name = app.tool_names.remove(tool_call_id.as_str());
            let tool_name = name.as_deref().unwrap_or("tool");
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                let (summary, status) = match outcome {
                    ToolCallOutcome::Completed => (
                        tool_completion_summary(tool_name, result, &CHILD_SUMMARY_FORMAT),
                        "done",
                    ),
                    ToolCallOutcome::Failed | ToolCallOutcome::Interrupted => {
                        (truncate_first_line(&result.content, 60), "failed")
                    },
                    ToolCallOutcome::Cancelled => (
                        format!("cancelled: {}", truncate_first_line(&result.content, 50)),
                        "cancelled",
                    ),
                };
                tracker.on_tool_completed(
                    tool_name,
                    &summary,
                    outcome.settled_call_failed(result),
                    &mut app.scrollback_queue,
                );
                app.status_text = format!("● Agent: {tool_name} {status}");
            }
        },
        EventPayload::Durable(DurableEventPayload::ErrorOccurred { message, .. })
        | EventPayload::Live(LiveEventPayload::ErrorOccurred { message, .. })
            if app.child_agents.contains_key(call_id) =>
        {
            app.scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Tool,
                text: format!("  ! {}", truncate_first_line(message, 80)),
            });
        },
        _ => {},
    }
}
