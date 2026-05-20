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
use crate::tui::{
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
    // 只处理当前活跃 session 的事件；子 session 的事件通过 ToolOutputDelta 在父上呈现。
    // SessionStarted 例外：它设置 active_session_id。
    if !matches!(&event.payload, EventPayload::SessionStarted { .. }) {
        if let Some(active) = &app.active_session_id {
            if event.session_id.as_str() != active.as_str() {
                return;
            }
        }
    }
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
            // Push a placeholder message so ToolOutputDelta can find it by key.
            app.push_message(
                crate::tui::store::transcript::MessageRole::Tool,
                human_action(tool_name).to_string(),
                String::new(),
                true,
                Some(call_id.to_string()),
            );
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
                .is_some_and(|m| m.label == "Task" || m.label.starts_with("Task("));
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
            let render_spec: Option<astrcode_core::render::RenderSpec> = result
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
                // Try to update the existing message from ToolCallStarted.
                let existing_idx = app
                    .messages
                    .iter()
                    .rposition(|m| m.key.as_deref() == Some(call_id.as_str()));

                if let Some(idx) = existing_idx {
                    let msg = &mut app.messages[idx];
                    msg.is_streaming = false;
                    if result.is_error {
                        msg.role = MessageRole::Error;
                    }
                    if let Some(spec) = render_spec.clone() {
                        msg.body.set_render(spec, result.content.clone());
                    } else {
                        msg.body.set_text(display_body.clone());
                    }
                    let completed = app.messages[idx].clone();
                    app.scrollback_queue
                        .push(ScrollbackEntry::Message(completed));
                } else if let Some(spec) = render_spec {
                    // No existing message — push new with RenderSpec.
                    let mut body =
                        crate::tui::store::transcript::MessageBody::text(result.content.clone());
                    body.set_render(spec, result.content.clone());
                    let msg = crate::tui::store::transcript::Message {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::{
        event::{Event, EventPayload},
        render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
        tool::ToolResult,
    };

    use super::*;
    use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

    fn make_app() -> App {
        App::new()
    }

    fn apply_payload(app: &mut App, payload: EventPayload) {
        let event = Event::new("session".into(), Some("turn".into()), payload);
        apply_event(app, &event);
    }

    fn tool_result(content: &str, is_error: bool) -> ToolResult {
        ToolResult {
            call_id: "call-1".into(),
            content: content.into(),
            is_error,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    fn tool_result_with_render(content: &str, spec: RenderSpec) -> ToolResult {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            UI_RENDER_METADATA_KEY.into(),
            serde_json::to_value(spec).unwrap(),
        );
        ToolResult {
            call_id: "call-1".into(),
            content: content.into(),
            is_error: false,
            error: None,
            metadata,
            duration_ms: None,
        }
    }

    #[test]
    fn search_tool_results_enter_transcript_as_summary() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
                result: tool_result("large search output", false),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Tool);
        assert_eq!(app.messages[0].label, "Search");
        // New system: display_body is "⎿ N line(s)" for non-error tools
        assert!(app.messages[0].body.plain_text().contains("⎿"));
    }

    #[test]
    fn shell_tool_results_still_enter_transcript() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                result: tool_result("command output", false),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Tool);
        // New system: display_body is "⎿ N line(s)"
        assert!(app.messages[0].body.plain_text().contains("⎿"));
    }

    #[test]
    fn hidden_tool_errors_still_enter_transcript() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "find".into(),
                result: tool_result("find failed", true),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Error);
        assert_eq!(app.messages[0].label, "Find");
        assert!(app.messages[0].body.plain_text().contains("find failed"));
    }

    #[test]
    fn hidden_tool_with_ui_render_enters_transcript() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
                result: tool_result_with_render(
                    "search complete",
                    RenderSpec::KeyValue {
                        entries: vec![RenderKeyValue {
                            key: "matches".into(),
                            value: "3".into(),
                            tone: RenderTone::Success,
                        }],
                        tone: RenderTone::Default,
                    },
                ),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Tool);
        assert!(app.messages[0].body.render_spec().is_some());
        assert_eq!(app.messages[0].body.plain_text(), "search complete");
    }

    #[test]
    fn assistant_deltas_enter_scrollback_incrementally() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "first line\nsecond".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::AssistantMessageCompleted {
                message_id: "msg-1".into(),
                text: "first line\nsecond".into(),
                reasoning_content: None,
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].body.plain_text(), "first line\nsecond");
        assert!(matches!(
            app.scrollback_queue.first(),
            Some(ScrollbackEntry::StreamHeader { label, .. }) if label == "Astrcode"
        ));
        assert!(
            app.scrollback_queue
                .last()
                .is_some_and(|e| matches!(e, ScrollbackEntry::BlankLine))
        );
        assert!(!app.scrollback_queue.iter().any(|e| {
            matches!(e, ScrollbackEntry::Message(m) if m.role == MessageRole::Assistant)
        }));
    }

    #[test]
    fn markdown_like_assistant_stream_is_not_reflowed_as_completed_message() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "# Title\n- item".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::AssistantMessageCompleted {
                message_id: "msg-1".into(),
                text: "# Title\n- item".into(),
                reasoning_content: None,
            },
        );
        assert!(!app.scrollback_queue.iter().any(|e| {
            matches!(e, ScrollbackEntry::Message(m) if m.role == MessageRole::Assistant)
        }));
    }

    #[test]
    fn agent_run_status_does_not_enter_scrollback() {
        let mut app = make_app();
        apply_payload(&mut app, EventPayload::AgentRunStarted);
        assert!(app.is_streaming);
        assert_eq!(app.status_text, "Agent running");
        assert!(app.scrollback_queue.is_empty());

        apply_payload(
            &mut app,
            EventPayload::AgentRunCompleted {
                reason: "done".into(),
            },
        );
        assert!(!app.is_streaming);
        assert!(app.scrollback_queue.is_empty());
    }

    #[test]
    fn normal_stop_reason_does_not_stick_in_idle_status() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        );
        assert_eq!(app.status_text, "Ready");
        apply_payload(
            &mut app,
            EventPayload::AgentRunCompleted {
                reason: "stop".into(),
            },
        );
        assert_eq!(app.status_text, "Ready");
    }

    #[test]
    fn actionable_completion_reason_stays_visible() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::AgentRunCompleted {
                reason: "aborted".into(),
            },
        );
        assert_eq!(app.status_text, "Ready · aborted");
    }

    #[test]
    fn input_history_recalls_prompts_and_commands() {
        let mut app = make_app();
        app.remember_input("first prompt");
        app.remember_input("/sessions");

        app.history_previous();
        assert_eq!(app.input_text(), "/sessions");
        assert!(app.show_slash_palette);

        app.history_previous();
        assert_eq!(app.input_text(), "first prompt");
        assert!(!app.show_slash_palette);

        app.history_next();
        assert_eq!(app.input_text(), "/sessions");

        app.history_next();
        assert!(app.input_text().is_empty());
    }

    #[test]
    fn child_agent_accumulates_text_and_shows_compact_tools() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-agent-1".into(),
                tool_name: "agent".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallRequested {
                call_id: "call-agent-1".into(),
                tool_name: "agent".into(),
                arguments: serde_json::json!({
                    "description": "探索设计",
                    "subagent_type": "explore",
                    "prompt": "探索项目"
                }),
            },
        );
        app.scrollback_queue.clear();

        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child assistant started\n".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "我来系统地探索项目中的设计。\n".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child tool started: find\n".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child tool completed: find: 3 files\n".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child tool started: read\n".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child assistant completed: 找到了相关文件\n".into(),
            },
        );

        let stream_texts: Vec<&str> = app
            .scrollback_queue
            .iter()
            .filter_map(|e| match e {
                ScrollbackEntry::StreamText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(stream_texts.len(), 3);
        assert_eq!(stream_texts[0], "我来系统地探索项目中的设计。");
        assert_eq!(stream_texts[1], "  · find");
        assert_eq!(stream_texts[2], "  · read");
        assert!(!stream_texts.iter().any(|t| t.contains("assistant")));
        assert!(!stream_texts.iter().any(|t| t.contains("tool completed")));
    }

    #[test]
    fn child_agent_tool_summary_on_completion() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-agent-2".into(),
                tool_name: "agent".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallRequested {
                call_id: "call-agent-2".into(),
                tool_name: "agent".into(),
                arguments: serde_json::json!({"description": "test"}),
            },
        );
        app.scrollback_queue.clear();

        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-agent-2".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "child tool started: find\nchild tool completed: find: ok\nchild tool \
                        started: find\nchild tool completed: find: ok\nchild tool started: \
                        grep\nchild tool completed: grep: 5 matches\n"
                    .into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-agent-2".into(),
                tool_name: "agent".into(),
                result: tool_result("探索完成", false),
            },
        );

        let summary = app.scrollback_queue.iter().find(
            |e| matches!(e, ScrollbackEntry::StreamText { text, .. } if text.contains("tool(s):")),
        );
        assert!(summary.is_some(), "should have tool summary");
        let text = match summary.unwrap() {
            ScrollbackEntry::StreamText { text, .. } => text.as_str(),
            _ => unreachable!(),
        };
        assert!(text.contains("3 tool(s)"));
        assert!(text.contains("find(2)"));
        assert!(text.contains("grep"));
        assert!(!app.child_agents.contains_key("call-agent-2"));
    }
}
