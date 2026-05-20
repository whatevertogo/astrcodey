//! Apply ClientNotification to App state.

use std::collections::BTreeMap;

use astrcode_core::{
    event::{Event, EventPayload},
    render::UI_RENDER_METADATA_KEY,
};
use astrcode_protocol::events::{
    ClientNotification, ExtensionCommandInfo, SessionListItem, SessionSnapshot,
};

use super::App;
use crate::tui_v2::{
    command::slash::SlashCommandSpec,
    store::transcript::{MessageRole, ScrollbackEntry},
    streaming::controller::StreamController,
};

pub fn apply(app: &mut App, notification: &ClientNotification) {
    match notification {
        ClientNotification::Event(event) => apply_event(app, event),
        ClientNotification::SessionResumed {
            session_id,
            snapshot,
        } => {
            apply_session_resumed(app, session_id, snapshot);
        },
        ClientNotification::SessionList { sessions } => apply_session_list(app, sessions),
        ClientNotification::UiRequest { message, .. } => {
            app.status_text = message.clone();
        },
        ClientNotification::Error { message, .. } => {
            app.show_error(message);
        },
        ClientNotification::ExtensionCommandList { commands } => {
            apply_extension_command_list(app, commands);
        },
        ClientNotification::ExtensionCommandResult {
            command_name,
            content,
            is_error,
        } => {
            let role = if *is_error {
                MessageRole::Error
            } else {
                MessageRole::System
            };
            let label = if *is_error {
                "Error"
            } else {
                command_name.as_str()
            };
            app.push_message(role, label.into(), content.clone(), false, None);
        },
    }
}

fn apply_event(app: &mut App, event: &Event) {
    match &event.payload {
        EventPayload::SessionStarted {
            working_dir,
            model_id,
            ..
        } => {
            app.active_session_id = Some(event.session_id.to_string());
            app.working_dir = working_dir.clone();
            app.model_name = model_id.clone();
            app.stream_states.clear();
            app.push_message(
                MessageRole::System,
                "Session".into(),
                format!("Created session {}", short_id(event.session_id.as_str())),
                false,
                None,
            );
            app.status_text = "Ready".into();
        },
        EventPayload::SessionDeleted => {
            app.active_session_id = None;
            app.status_text = "Session deleted".into();
        },
        EventPayload::TurnStarted => {
            app.is_streaming = true;
            app.error = None;
            app.status_text = "Working".into();
        },
        EventPayload::TurnCompleted { finish_reason } => {
            app.is_streaming = false;
            app.status_text = ready_status(finish_reason);
        },
        EventPayload::AgentRunStarted => {
            app.is_streaming = true;
            app.status_text = "Agent running".into();
        },
        EventPayload::AgentRunCompleted { reason } => {
            app.is_streaming = false;
            app.status_text = ready_status(reason);
        },
        EventPayload::UserMessage { .. } => {
            // Optimistically pushed on Enter; skip.
        },
        EventPayload::AssistantMessageStarted { message_id } => {
            let width = 120; // TODO: get from terminal width
            app.stream_states
                .insert(message_id.to_string(), StreamController::new(Some(width)));
            app.scrollback_queue.push(ScrollbackEntry::StreamHeader {
                role: MessageRole::Assistant,
                label: "Astrcode".into(),
            });
            app.push_message(
                MessageRole::Assistant,
                "Astrcode".into(),
                String::new(),
                true,
                Some(message_id.to_string()),
            );
            app.status_text = "Working".into();
            tracing::debug!(message_id = %message_id, "stream_open");
        },
        EventPayload::AssistantTextDelta { message_id, delta } => {
            if let Some(msg) = app.find_message_mut(message_id.as_str()) {
                msg.body.append_text(delta);
            }
            if let Some(ctrl) = app.stream_states.get_mut(message_id.as_str()) {
                let theme = app.theme.clone();
                if ctrl.push_delta(delta, &theme) {
                    // Lines are queued; commit_tick will drain them.
                }
            }
            tracing::debug!(message_id = %message_id, len = delta.len(), "stream_chunk");
        },
        EventPayload::AssistantMessageCompleted {
            message_id, text, ..
        } => {
            let theme = app.theme.clone();
            let lines = if let Some(ctrl) = app.stream_states.remove(message_id.as_str()) {
                let mut ctrl = ctrl;
                ctrl.finalize(text, &theme)
            } else {
                Vec::new()
            };
            for line in lines {
                app.scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Assistant,
                    text: line.spans.iter().map(|s| s.content.as_ref()).collect(),
                });
            }
            app.scrollback_queue.push(ScrollbackEntry::BlankLine);
            if let Some(msg) = app.find_message_mut(message_id.as_str()) {
                msg.body.set_text(text.clone());
                msg.is_streaming = false;
            }
            tracing::debug!(message_id = %message_id, "stream_close");
        },
        EventPayload::ThinkingDelta { delta, .. } => {
            app.status_text = format!("Thinking · {}", delta);
        },
        EventPayload::ToolCallStarted { call_id, tool_name } => {
            app.status_text = format!("Running {}", human_action(tool_name));
            tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_open");
        },
        EventPayload::ToolCallRequested {
            call_id: _,
            tool_name,
            arguments: _,
        } => {
            app.status_text = format!("Running {}", human_action(tool_name));
        },
        EventPayload::ToolOutputDelta { call_id, delta, .. } => {
            // Check if this is an agent tool (child agent output).
            let is_agent = app
                .messages
                .iter()
                .rev()
                .find(|m| m.key.as_deref() == Some(call_id.as_str()))
                .is_some_and(|m| m.label.starts_with("Task("));
            if is_agent {
                let tracker = app.child_agents.entry(call_id.to_string()).or_default();
                tracker.handle_delta(delta, &mut app.scrollback_queue);
            } else {
                app.status_text = "Receiving output".into();
            }
        },
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => {
            let render_spec = result
                .metadata
                .get(UI_RENDER_METADATA_KEY)
                .and_then(|v| serde_json::from_value(v.clone()).ok());

            let display_body = if result.is_error {
                let err = result
                    .error
                    .clone()
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or_else(|| result.content.clone());
                format!("⎿ error: {err}")
            } else if result.content.trim().is_empty() {
                "⎿ done".into()
            } else {
                format!("⎿ {} line(s)", result.content.lines().count())
            };

            let should_print = !matches!(tool_name.as_str(), "tool_search")
                || result.is_error
                || render_spec.is_some();

            if should_print {
                if let Some(spec) = render_spec {
                    let mut body =
                        crate::tui_v2::store::transcript::MessageBody::text(result.content.clone());
                    body.set_render(spec, result.content.clone());
                    let msg = crate::tui_v2::store::transcript::Message {
                        role: if result.is_error {
                            MessageRole::Error
                        } else {
                            MessageRole::Tool
                        },
                        label: human_action(tool_name).to_string(),
                        body,
                        is_streaming: false,
                        key: Some(call_id.to_string()),
                    };
                    app.scrollback_queue
                        .push(ScrollbackEntry::Message(msg.clone()));
                    app.messages.push(msg);
                } else if result.is_error || !display_body.is_empty() {
                    app.push_message(
                        if result.is_error {
                            MessageRole::Error
                        } else {
                            MessageRole::Tool
                        },
                        human_action(tool_name).to_string(),
                        display_body,
                        false,
                        Some(call_id.to_string()),
                    );
                }
            }

            if tool_name == "agent" {
                if let Some(mut tracker) = app.child_agents.remove(call_id.as_str()) {
                    tracker.flush_on_completion(&mut app.scrollback_queue);
                }
            }

            app.status_text = format!("{} completed", human_action(tool_name));
            tracing::debug!(call_id = %call_id, tool = %tool_name, is_error = result.is_error, "tool_close");
        },
        EventPayload::CompactionStarted => {
            app.push_message(
                MessageRole::System,
                "System".into(),
                "Compacting context...".into(),
                true,
                Some("compaction".into()),
            );
        },
        EventPayload::ErrorOccurred { message, .. } => {
            app.show_error(message);
        },
        EventPayload::ModelIdChanged { model_id } => {
            app.model_name = model_id.clone();
        },
        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            ..
        } => {
            app.push_message(
                MessageRole::System,
                "Agent".into(),
                format!(
                    "spawned {} — {} ({})",
                    agent_name,
                    task,
                    short_id(child_session_id.as_str())
                ),
                false,
                None,
            );
        },
        EventPayload::AgentSessionCompleted {
            child_session_id,
            summary,
            ..
        } => {
            app.push_message(
                MessageRole::System,
                "Agent".into(),
                format!(
                    "completed ({}) — {}",
                    short_id(child_session_id.as_str()),
                    summary
                ),
                false,
                None,
            );
        },
        EventPayload::AgentSessionFailed {
            child_session_id,
            error,
            ..
        } => {
            app.push_message(
                MessageRole::System,
                "Agent".into(),
                format!(
                    "failed ({}) — {}",
                    short_id(child_session_id.as_str()),
                    error
                ),
                false,
                None,
            );
        },
        EventPayload::ToolCallBackgrounded {
            tool_name, task_id, ..
        } => {
            app.status_text = format!("{} → background ({})", tool_name, task_id);
        },
        EventPayload::BackgroundTaskCompleted {
            task_id, tool_name, ..
        } => {
            app.status_text = format!("{} background done ({})", tool_name, task_id);
        },
        _ => {},
    }
}

fn apply_session_resumed(app: &mut App, session_id: &str, snapshot: &SessionSnapshot) {
    app.active_session_id = Some(session_id.to_string());
    app.working_dir = snapshot.working_dir.clone();
    app.messages.clear();
    app.stream_states.clear();
    app.child_agents.clear();

    for message in &snapshot.messages {
        let role = match message.role.as_str() {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            _ => MessageRole::System,
        };
        let label = match &role {
            MessageRole::User => "You",
            MessageRole::Assistant => "Astrcode",
            MessageRole::System => "System",
            MessageRole::Tool => "Tool",
            MessageRole::Error => "Error",
        };
        app.push_message(role, label.into(), message.content.clone(), false, None);
    }
    app.status_text = format!("Resumed {}", short_id(session_id));
    tracing::debug!(session_id = %session_id, messages = snapshot.messages.len(), "resume_snapshot");
}

fn apply_session_list(app: &mut App, sessions: &[SessionListItem]) {
    app.available_sessions = sessions.iter().map(|s| s.session_id.clone()).collect();
    app.status_text = format!("{} session(s)", sessions.len());
    let body = if sessions.is_empty() {
        "No sessions".into()
    } else {
        sessions
            .iter()
            .map(|s| {
                let marker = if app.active_session_id.as_deref() == Some(s.session_id.as_str()) {
                    "*"
                } else {
                    " "
                };
                let dir = if s.working_dir.is_empty() {
                    "unknown".into()
                } else {
                    astrcode_support::text::compact_inline(&s.working_dir, 72)
                };
                format!("{marker} {} · {dir}", short_id(&s.session_id))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    app.push_message(MessageRole::System, "Sessions".into(), body, false, None);
}

fn apply_extension_command_list(app: &mut App, commands: &[ExtensionCommandInfo]) {
    app.extension_commands = commands
        .iter()
        .map(|info| SlashCommandSpec {
            name: info.name.clone(),
            usage: format!("/{}", info.name),
            description: info.description.clone(),
            needs_argument: info.needs_argument,
        })
        .collect();
    app.status_text = format!("{} extension command(s) loaded", commands.len());
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn ready_status(reason: &str) -> String {
    if reason == "stop" {
        "Ready".into()
    } else {
        format!("Ready · {reason}")
    }
}

fn human_action(tool_name: &str) -> &str {
    match tool_name {
        "shell" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "find" => "Find",
        "grep" => "Search",
        "patch" => "Patch",
        "agent" => "Task",
        other => other,
    }
}
