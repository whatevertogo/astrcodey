//! Apply ClientNotification to App state.

use std::collections::BTreeMap;

use astrcode_core::{
    event::{Event, EventPayload},
    render::UI_RENDER_METADATA_KEY,
};
use astrcode_protocol::events::{
    ClientNotification, ExtensionCommandInfo, SessionListItem, SessionSnapshot, UiRequestKind,
};
use astrcode_support::text::truncate_first_line;

use super::App;
use crate::tui::{
    command::slash::SlashCommandSpec,
    ext::tool::ToolRenderCtx,
    store::transcript::{Message, MessageBody, MessageRole, ScrollbackEntry},
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
        ClientNotification::UiRequest {
            request_id,
            kind,
            message,
            options,
            ..
        } => apply_ui_request(app, request_id, kind, message, options.as_deref()),
        ClientNotification::Error { message, .. } => {
            app.show_error(message);
        },
        ClientNotification::ExtensionCommandList {
            commands,
            keybindings,
            status_items,
        } => {
            apply_extension_command_list(app, commands, keybindings, status_items);
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
        ClientNotification::StatusItemUpdate { id, text } => {
            if text.is_empty() {
                app.status_items.remove(id);
            } else {
                app.status_items.insert(id.clone(), text.clone());
            }
        },
        ClientNotification::ExtensionRegistryChanged => {
            app.extension_commands.clear();
            app.keybindings.clear();
            app.status_items.clear();
            app.needs_extension_refresh = true;
            app.status_text = "Extension registry changed".into();
        },
    }
}

fn apply_event(app: &mut App, event: &Event) {
    // 只处理当前活跃 session 的事件；子 session 的事件通过直接路由到 child_agent tracker。
    // SessionStarted 例外：它设置 active_session_id。
    if !matches!(&event.payload, EventPayload::SessionStarted { .. }) {
        if let Some(active) = &app.active_session_id {
            if event.session_id.as_str() != active.as_str() {
                // 检查是否是已跟踪的子 session 事件
                if let Some(call_id) = app
                    .child_session_map
                    .get(event.session_id.as_str())
                    .cloned()
                {
                    apply_child_session_event(app, &call_id, event);
                }
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
            // 不立刻写 StreamHeader，延迟到第一个 AssistantTextDelta 时再写，
            // 避免模型直接走 tool_call 时留下空块。
            app.push_message(
                MessageRole::Assistant,
                "Astrcode".into(),
                String::new(),
                true,
                Some(message_id.to_string()),
            );
            app.status_text = "Thinking".into();
            tracing::debug!(message_id = %message_id, "stream_open");
        },
        EventPayload::AssistantTextDelta { message_id, delta } => {
            // 第一次收到 text delta 时写入 StreamHeader
            let is_first_delta = app
                .find_message_mut(message_id.as_str())
                .is_some_and(|msg| msg.body.is_empty());
            if is_first_delta {
                app.scrollback_queue.push(ScrollbackEntry::StreamHeader {
                    role: MessageRole::Assistant,
                    label: "Astrcode".into(),
                });
                app.status_text = "Working".into();
            }
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
            let has_visible_content = !lines.is_empty() || !text.trim().is_empty();
            for line in lines {
                app.scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: MessageRole::Assistant,
                    text: line.spans.iter().map(|s| s.content.as_ref()).collect(),
                });
            }
            // Only add blank separator when there's visible content (avoid gaps between tool
            // calls when LLM returns empty text before issuing more tool calls).
            if has_visible_content {
                app.scrollback_queue.push(ScrollbackEntry::BlankLine);
            }
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
            // Codex style: only update status bar. Don't push to scrollback yet.
            // We track the tool internally for later completion display.
            app.status_text = format!("● {}", tool_call_summary(tool_name, None));
            // Store a placeholder in messages so child-agent detection works.
            app.push_message(
                MessageRole::Tool,
                human_action(tool_name).to_string(),
                String::new(),
                true,
                Some(call_id.to_string()),
            );
            // Remove the auto-pushed scrollback entry (we don't want streaming tools in
            // scrollback).
            app.scrollback_queue.retain(|e| {
                !matches!(e, ScrollbackEntry::Message(m) if m.key.as_deref() == Some(call_id.as_str()))
            });
            // TODO：A BETTER WAY MAYBE LATER
            // For agent tool: create tracker and show a header in scrollback.
            if tool_name == "agent" {
                app.child_agents.insert(
                    call_id.to_string(),
                    crate::tui::store::child_agent::ChildAgentTracker::default(),
                );
                app.scrollback_queue.push(ScrollbackEntry::StreamHeader {
                    role: MessageRole::Tool,
                    label: "Task".into(),
                });
            }
            tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_open");
        },
        EventPayload::ToolCallRequested {
            call_id: _,
            tool_name,
            arguments,
        } => {
            // Update status with argument summary.
            app.status_text = format!("● {}", tool_call_summary(tool_name, Some(arguments)));
        },
        EventPayload::ToolOutputDelta { .. } => {
            // 父 session 的非 agent 工具输出——更新 status bar 即可。
            // 子 agent 的工具进度由 apply_child_session_event 直接处理。
            app.status_text = "● Receiving output".to_string();
        },
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => {
            // Codex style: show one compact line in scrollback for the completed tool.
            // Format: "● Ran <command>" or "✗ <error>" or "● Task completed"

            // Remove the streaming placeholder from messages.
            if let Some(idx) = app
                .messages
                .iter()
                .rposition(|m| m.key.as_deref() == Some(call_id.as_str()))
            {
                app.messages.remove(idx);
            }

            if tool_name == "agent" {
                // Sub-agent: flush tracker output and show summary.
                if let Some(mut tracker) = app.child_agents.remove(call_id.as_str()) {
                    tracker.flush_on_completion(&mut app.scrollback_queue);
                }
                // 清理 child_session_map 中引用该 call_id 的条目
                app.child_session_map.retain(|_, v| v != call_id.as_str());
                let summary = if result.is_error {
                    format!(
                        "✗ Task failed: {}",
                        truncate_first_line(&result.content, 80)
                    )
                } else if result.content.trim().is_empty() {
                    "● Task completed".into()
                } else {
                    format!(
                        "● Task completed — {}",
                        truncate_first_line(&result.content, 60)
                    )
                };
                app.push_message(
                    if result.is_error {
                        MessageRole::Error
                    } else {
                        MessageRole::Tool
                    },
                    "Task".into(),
                    summary,
                    false,
                    None,
                );
            } else if result.is_error {
                // Error: always show.
                let err = result
                    .error
                    .clone()
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or_else(|| result.content.clone());
                app.push_message(
                    MessageRole::Error,
                    human_action(tool_name).to_string(),
                    format!("✗ {}", truncate_first_line(&err, 100)),
                    false,
                    None,
                );
            } else {
                // Try custom tool renderer for rich display.
                if let Some(renderer) = app.tool_renderers.get(tool_name) {
                    let mut state: Box<dyn std::any::Any + Send> = Box::new(());
                    let mut ctx = ToolRenderCtx {
                        call_id: call_id.as_str(),
                        tool_name,
                        args: None,
                        args_complete: true,
                        execution_started: true,
                        is_partial: false,
                        is_error: false,
                        expanded: false,
                        state: &mut state,
                    };
                    if let Some(spec) = renderer.render_result(result, &mut ctx) {
                        let fallback = tool_completion_summary(tool_name, result);
                        app.push_rendered_message(
                            MessageRole::Tool,
                            human_action(tool_name).to_string(),
                            spec,
                            fallback,
                            false,
                            None,
                        );
                        app.status_text = "Ready".into();
                        tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_rendered");
                        return;
                    }
                }
                // Fallback: compact one-line summary (codex style).
                let summary = tool_completion_summary(tool_name, result);
                app.push_message(
                    MessageRole::Tool,
                    human_action(tool_name).to_string(),
                    summary,
                    false,
                    None,
                );
            }

            app.status_text = "Ready".into();
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
        EventPayload::RecapGenerated { text, .. } => {
            app.push_message(
                MessageRole::System,
                "Recap".into(),
                text.clone(),
                false,
                None,
            );
            app.status_text = "Ready".into();
        },
        EventPayload::ModelIdChanged { model_id } => {
            app.model_name = model_id.clone();
        },
        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_call_id,
            ..
        } => {
            let short_task = truncate_first_line(task, 60);
            app.push_message(
                MessageRole::System,
                format!("Agent({agent_name})"),
                short_task,
                false,
                None,
            );
            app.status_text = format!("● Agent: {agent_name}");

            // 精确建立 child_session_id → call_id 映射。
            app.child_session_map
                .insert(child_session_id.to_string(), tool_call_id.to_string());
        },
        EventPayload::AgentSessionCompleted {
            child_session_id,
            summary,
            ..
        } => {
            let short_summary = truncate_first_line(summary, 60);
            app.push_message(
                MessageRole::Tool,
                "Agent".into(),
                format!("● Done — {short_summary}"),
                false,
                None,
            );
            app.child_session_map.remove(child_session_id.as_str());
            app.status_text = "Ready".into();
        },
        EventPayload::AgentSessionFailed {
            child_session_id,
            error,
            ..
        } => {
            app.push_message(
                MessageRole::Error,
                "Agent".into(),
                format!("✗ {}", truncate_first_line(error, 80)),
                false,
                None,
            );
            app.child_session_map.remove(child_session_id.as_str());
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
        EventPayload::Custom { name, data } => {
            // 将自定义事件作为带 custom_type 的消息推入 scrollback。
            // 如果 MessageRendererRegistry 中有匹配的渲染器，渲染时会分发给它；
            // 否则降级为纯文本（名称 + JSON 预览）。
            let fallback = format!(
                "[{name}] {}",
                astrcode_support::text::compact_inline(&data.to_string(), 80)
            );
            let body = MessageBody::with_custom(name.clone(), data.clone(), fallback);
            let msg = Message {
                role: MessageRole::System,
                label: name.clone(),
                body,
                is_streaming: false,
                key: None,
            };
            app.scrollback_queue
                .push(ScrollbackEntry::Message(msg.clone()));
            app.messages.push(msg);
        },
        _ => {},
    }
}

/// 处理来自子 session 的事件，将工具调用进度路由到对应的 ChildAgentTracker。
fn apply_child_session_event(app: &mut App, call_id: &str, event: &Event) {
    match &event.payload {
        EventPayload::ToolCallStarted { tool_name, .. } => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                tracker.on_tool_started(tool_name);
                app.status_text = format!("●Task → {tool_name}");
            }
        },
        EventPayload::ToolCallCompleted {
            tool_name, result, ..
        } => {
            if let Some(tracker) = app.child_agents.get_mut(call_id) {
                let summary = child_tool_summary(tool_name, result);
                tracker.on_tool_completed(
                    tool_name,
                    &summary,
                    result.is_error,
                    &mut app.scrollback_queue,
                );
                app.status_text = format!("●Agent: {tool_name} done");
            }
        },
        EventPayload::ErrorOccurred { message, .. } if app.child_agents.contains_key(call_id) => {
            app.scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Tool,
                text: format!("  ! {}", truncate_first_line(message, 80)),
            });
        },
        _ => {},
    }
}

/// 子 agent 工具完成的简短摘要。
fn child_tool_summary(tool_name: &str, result: &astrcode_core::tool::ToolResult) -> String {
    let content = result.content.trim();
    if result.is_error {
        return truncate_first_line(result.error.as_deref().unwrap_or(content), 60);
    }
    match tool_name {
        "shell" => {
            let line_count = content.lines().count();
            if line_count <= 1 && !content.is_empty() {
                truncate_first_line(content, 50)
            } else if line_count > 1 {
                format!("{line_count} lines of output")
            } else {
                "done".into()
            }
        },
        "read" => {
            if content.is_empty() {
                "done".into()
            } else {
                format!("{} line(s)", content.lines().count())
            }
        },
        "write" | "edit" | "patch" => "done".into(),
        "find" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{count} file(s)")
        },
        "grep" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{count} match(es)")
        },
        _ => {
            if content.is_empty() {
                "done".into()
            } else {
                truncate_first_line(content, 50)
            }
        },
    }
}

fn apply_session_resumed(app: &mut App, session_id: &str, snapshot: &SessionSnapshot) {
    app.active_session_id = Some(session_id.to_string());
    app.working_dir = snapshot.working_dir.clone();
    app.messages.clear();
    app.needs_terminal_reset = true;
    app.stream_states.clear();
    app.child_agents.clear();
    app.child_session_map.clear();

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
    use crate::tui::app::SessionEntry;
    app.available_sessions = sessions
        .iter()
        .map(|s| {
            let title = s
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| astrcode_support::text::compact_inline(t, 60))
                .unwrap_or_else(|| short_id(&s.session_id).to_string());
            SessionEntry {
                session_id: s.session_id.clone(),
                title,
                working_dir: s.working_dir.clone(),
                is_child: s.parent_session_id.is_some(),
                last_active_at: s.last_active_at.clone(),
            }
        })
        .collect();
    app.status_text = format!("{} session(s)", sessions.len());

    // 如果 session_picker 处于打开状态，刷新 picker 内容（仅当前项目的 session）
    if app.session_picker.is_some() {
        app.open_session_picker();
    }
}

fn apply_ui_request(
    app: &mut App,
    request_id: &str,
    kind: &UiRequestKind,
    message: &str,
    options: Option<&[String]>,
) {
    match (kind, options) {
        (UiRequestKind::Select, Some(options)) if !options.is_empty() => {
            app.open_ui_picker(
                request_id.to_string(),
                message.to_string(),
                options.to_vec(),
            );
        },
        _ => {
            app.status_text = message.to_string();
        },
    }
}

fn apply_extension_command_list(
    app: &mut App,
    commands: &[ExtensionCommandInfo],
    keybindings: &[astrcode_protocol::events::KeybindingInfoDto],
    status_items: &[astrcode_protocol::events::StatusItemInfoDto],
) {
    app.extension_commands = commands
        .iter()
        .map(|info| SlashCommandSpec {
            name: info.name.clone(),
            usage: format!("/{}", info.name),
            description: info.description.clone(),
            needs_argument: info.needs_argument,
        })
        .collect();
    // 注册插件快捷键
    app.keybindings = keybindings
        .iter()
        .map(|kb| crate::tui::keybinding::RegisteredKeybinding {
            key: kb.key.clone(),
            command: kb.command.clone(),
            arguments: kb.arguments.clone(),
        })
        .collect();
    // 初始化状态栏项
    for item in status_items {
        app.status_items.insert(item.id.clone(), item.text.clone());
    }
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

/// Codex-style one-line tool call summary for the status bar.
fn tool_call_summary(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    let action = human_action(tool_name);
    match tool_name {
        "shell" => {
            let cmd = arguments
                .and_then(|a| a["command"].as_str())
                .unwrap_or("...");
            format!("Running  $ {}", truncate_first_line(cmd, 60))
        },
        "read" => {
            let path = arguments.and_then(|a| a["path"].as_str()).unwrap_or("...");
            format!("Reading {path}")
        },
        "write" | "edit" => {
            let path = arguments.and_then(|a| a["path"].as_str()).unwrap_or("...");
            format!("{action} {path}")
        },
        "find" => {
            let pattern = arguments
                .and_then(|a| a["pattern"].as_str())
                .unwrap_or("...");
            format!("Finding {pattern}")
        },
        "grep" => {
            let query = arguments
                .and_then(|a| a["pattern"].as_str().or(a["query"].as_str()))
                .unwrap_or("...");
            format!("Searching {query}")
        },
        "agent" => {
            let desc = arguments
                .and_then(|a| a["description"].as_str())
                .unwrap_or("subtask");
            format!("Task: {desc}")
        },
        _ => format!("{action}..."),
    }
}

/// Codex-style one-line tool completion summary for scrollback.
fn tool_completion_summary(tool_name: &str, result: &astrcode_core::tool::ToolResult) -> String {
    let content = result.content.trim();
    match tool_name {
        "shell" => {
            if content.is_empty() {
                "● Ran (no output)".into()
            } else {
                let line_count = content.lines().count();
                if line_count <= 1 {
                    format!("● {}", truncate_first_line(content, 80))
                } else {
                    format!("● ({line_count} lines of output)")
                }
            }
        },
        "read" => format!("● Read {} line(s)", content.lines().count().max(1)),
        "write" | "edit" | "patch" => "● Done".into(),
        "find" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("● Found {count} file(s)")
        },
        "grep" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("● {count} match(es)")
        },
        _ => {
            if content.is_empty() {
                "● Done".into()
            } else {
                format!("● {}", truncate_first_line(content, 60))
            }
        },
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
        App::new(crate::tui::theme::Theme::detect())
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
}

#[cfg(test)]
mod codex_style_tests {
    use std::collections::BTreeMap;

    use astrcode_core::{
        event::{Event, EventPayload},
        tool::ToolResult,
    };

    use super::*;
    use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

    fn make_app() -> App {
        App::new(crate::tui::theme::Theme::detect())
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

    #[test]
    fn tool_completion_shows_compact_summary() {
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
                result: tool_result("match1\nmatch2\nmatch3", false),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].body.plain_text().contains("● 3 match"));
    }

    #[test]
    fn tool_error_shows_in_transcript() {
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
                result: tool_result("permission denied", true),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Error);
        assert!(app.messages[0].body.plain_text().contains("✗"));
    }

    #[test]
    fn agent_tool_shows_compact_task_summary() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-agent".into(),
                tool_name: "agent".into(),
            },
        );
        apply_payload(
            &mut app,
            EventPayload::ToolCallCompleted {
                call_id: "call-agent".into(),
                tool_name: "agent".into(),
                result: tool_result("Found 3 relevant files", false),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert!(
            app.messages[0]
                .body
                .plain_text()
                .contains("● Task completed")
        );
    }

    #[test]
    fn tool_output_delta_only_updates_status() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
            },
        );
        app.scrollback_queue.clear();
        apply_payload(
            &mut app,
            EventPayload::ToolOutputDelta {
                call_id: "call-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "lots of output\n".into(),
            },
        );
        assert!(app.scrollback_queue.is_empty());
        assert!(app.status_text.contains("Receiving"));
    }
}
