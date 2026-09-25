//! Route child-session events to the owning tool-call tracker.

use astrcode_core::event::{DurableEventPayload, Event, EventPayload, LiveEventPayload};

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
        EventPayload::Live(LiveEventPayload::ToolCallStarted { tool_name, .. }) => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                tracker.on_tool_started(tool_name);
                app.status_text = format!("● Task → {tool_name}");
            }
        },
        EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
            tool_name, result, ..
        }) => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                let summary = tool_completion_summary(tool_name, result, &CHILD_SUMMARY_FORMAT);
                tracker.on_tool_completed(
                    tool_name,
                    &summary,
                    result.is_error,
                    &mut app.scrollback_queue,
                );
                app.status_text = format!("● Agent: {tool_name} done");
            }
        },
        EventPayload::Durable(DurableEventPayload::ToolCallFailed {
            tool_name, error, ..
        }) => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                tracker.on_tool_completed(
                    tool_name,
                    &truncate_first_line(error, 60),
                    true,
                    &mut app.scrollback_queue,
                );
                app.status_text = format!("● Agent: {tool_name} failed");
            }
        },
        EventPayload::Durable(DurableEventPayload::ToolCallCancelled {
            tool_name, reason, ..
        }) => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                tracker.on_tool_completed(
                    tool_name,
                    &format!("cancelled: {}", truncate_first_line(reason, 50)),
                    true,
                    &mut app.scrollback_queue,
                );
                app.status_text = format!("● Agent: {tool_name} cancelled");
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
