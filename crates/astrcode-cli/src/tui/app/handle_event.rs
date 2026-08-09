//! Apply ClientNotification to App state.

use astrcode_context::is_compact_summary_text;
use astrcode_core::event::{
    CustomEventData, DurableEventPayload, Event, EventPayload, LiveEventPayload,
};
use astrcode_protocol::events::{
    ClientNotification, ExtensionCommandInfoDto, KeybindingDto, SessionListItemDto,
    SessionSnapshot, UiRequestKind,
};

use super::App;
use crate::tui::{
    command::slash::SlashCommandSpec,
    ext::tool::ToolRenderCtx,
    render::inline_preview,
    store::transcript::{Message, MessageBody, MessageRole, ScrollbackEntry},
    streaming::controller::StreamController,
    tool_vocab::tool_display_name,
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
            app.extension_command_names.clear();
            app.keybindings.clear();
            app.status_items.clear();
            app.needs_extension_refresh = true;
            app.status_text = "Extension registry changed".into();
        },
        // TUI 已从 all_notifications 收到原始扩展事件，无需处理桌面端全局副本。
        ClientNotification::GlobalCustomEvent { .. } => {},
    }
}

fn apply_event(app: &mut App, event: &Event) {
    // 只处理当前活跃 session 的事件；子 session 的事件通过直接路由到 child_agent tracker。
    // SessionStarted 例外：它设置 active_session_id。
    if !matches!(
        &event.payload,
        EventPayload::Durable(DurableEventPayload::SessionStarted(_))
    ) {
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
        EventPayload::Durable(DurableEventPayload::SessionStarted(started)) => {
            app.active_session_id = Some(event.session_id.to_string());
            app.working_dir = started.working_dir.clone();
            app.model_name = started.model_id.clone();
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
        EventPayload::Durable(DurableEventPayload::TurnStarted) => {
            app.is_streaming = true;
            app.error = None;
            app.status_text = "Working".into();
        },
        EventPayload::Durable(DurableEventPayload::TurnCompleted { finish_reason }) => {
            app.is_streaming = false;
            app.status_text = ready_status(finish_reason);
        },
        EventPayload::Live(LiveEventPayload::AgentRunStarted) => {
            app.is_streaming = true;
            app.status_text = "Agent running".into();
        },
        EventPayload::Live(LiveEventPayload::AgentRunCompleted { reason }) => {
            app.is_streaming = false;
            app.status_text = ready_status(reason);
        },
        EventPayload::Live(LiveEventPayload::LlmRetrying {
            attempt,
            max_retries,
            ..
        }) => {
            app.status_text = format!("Reconnecting · {attempt}/{max_retries}");
        },
        EventPayload::Live(LiveEventPayload::LlmRetryRecovered) => {
            app.status_text = "Thinking".into();
        },
        EventPayload::Durable(DurableEventPayload::UserMessage { .. }) => {
            // Optimistically pushed on Enter; skip.
        },
        EventPayload::Live(LiveEventPayload::AssistantMessageStarted { message_id }) => {
            let width = app.content_width;
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
        EventPayload::Live(LiveEventPayload::AssistantMessageReset { message_id }) => {
            let width = app.content_width;
            app.stream_states
                .insert(message_id.to_string(), StreamController::new(Some(width)));
            app.pending_assistant_stream_reset = Some(message_id.to_string());
            app.scrollback_queue
                .retain(|entry| entry.assistant_message_id() != Some(message_id.as_str()));
            if let Some(message) = app.find_message_mut(message_id.as_str()) {
                message.body.set_text(String::new());
            }
            tracing::debug!(message_id = %message_id, "stream_reset");
        },
        EventPayload::Live(LiveEventPayload::AssistantTextDelta { message_id, delta }) => {
            // 第一次收到 text delta 时写入 StreamHeader
            let is_first_delta = app
                .find_message_mut(message_id.as_str())
                .is_some_and(|msg| msg.body.is_empty());
            if is_first_delta {
                app.scrollback_queue
                    .push(ScrollbackEntry::AssistantStreamHeader {
                        message_id: message_id.to_string(),
                    });
                app.status_text = "Working".into();
            }
            if let Some(msg) = app.find_message_mut(message_id.as_str()) {
                msg.body.append_text(delta);
            }
            if let Some(ctrl) = app.stream_states.get_mut(message_id.as_str()) {
                if ctrl.push_delta(delta) {
                    // Lines are queued; commit_tick will drain them.
                }
            }
            tracing::debug!(message_id = %message_id, len = delta.len(), "stream_chunk");
        },
        EventPayload::Durable(DurableEventPayload::AssistantMessageCompleted {
            message_id,
            text,
            ..
        }) => {
            let lines = if let Some(ctrl) = app.stream_states.remove(message_id.as_str()) {
                let mut ctrl = ctrl;
                ctrl.finalize(text)
            } else {
                Vec::new()
            };
            let has_visible_content = !lines.is_empty() || !text.trim().is_empty();
            for line in lines {
                app.scrollback_queue
                    .push(ScrollbackEntry::AssistantStreamText {
                        message_id: message_id.to_string(),
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
        EventPayload::Live(LiveEventPayload::ThinkingDelta { delta, .. }) => {
            app.status_text = format!("Thinking · {}", delta);
        },
        EventPayload::Live(LiveEventPayload::ToolCallStarted { call_id, tool_name }) => {
            // Codex style: only update status bar. Don't push to scrollback yet.
            // We track the tool internally for later completion display.
            app.status_text = format!("● {}", tool_call_summary(tool_name, None));
            // Store a placeholder in messages so child-agent detection works.
            app.push_message(
                MessageRole::Tool,
                tool_display_name(tool_name).to_string(),
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
                app.scrollback_queue.push(ScrollbackEntry::StreamHeader);
            }
            tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_open");
        },
        EventPayload::Durable(DurableEventPayload::ToolApprovalRequested {
            call_id,
            tool_name,
            prompt,
            ..
        }) => {
            app.pending_tool_approval = Some(crate::tui::app::ToolApprovalPrompt {
                call_id: call_id.to_string(),
                tool_name: tool_name.clone(),
            });
            app.status_text = format!("⚠ Approval required: {tool_name} — y/n");
            app.push_message(
                MessageRole::System,
                "Approval".into(),
                format!(
                    "Tool `{tool_name}` requires approval:\n{prompt}\nPress y to allow once, n to \
                     deny."
                ),
                false,
                None,
            );
        },
        EventPayload::Durable(DurableEventPayload::ToolApprovalResolved { call_id, .. }) => {
            if app
                .pending_tool_approval
                .as_ref()
                .is_some_and(|pending| pending.call_id == call_id.as_str())
            {
                app.pending_tool_approval = None;
            }
        },
        EventPayload::Durable(DurableEventPayload::ToolCallRequested {
            call_id: _,
            tool_name,
            arguments,
            ..
        }) => {
            // Update status with argument summary.
            app.status_text = format!("● {}", tool_call_summary(tool_name, Some(arguments)));
        },
        EventPayload::Live(LiveEventPayload::ToolOutputDelta { .. }) => {
            // 父 session 的非 agent 工具输出——更新 status bar 即可。
            // 子 agent 的工具进度由 apply_child_session_event 直接处理。
            app.status_text = "● Receiving output".to_string();
        },
        EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            ..
        }) => {
            // Codex style: show one compact line in scrollback for the completed tool.
            // Format: "● Ran <command>" or "✗ <error>" or "● Task completed"
            close_tool_call_state(app, call_id.as_str());

            if tool_name == "agent" {
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
                    tool_display_name(tool_name).to_string(),
                    format!("✗ {}", truncate_first_line(&err, 100)),
                    false,
                    None,
                );
            } else {
                // Try custom tool renderer for rich display.
                if let Some(renderer) = app.tool_renderers.get(tool_name) {
                    let ctx = ToolRenderCtx { tool_name };
                    if let Some(spec) = renderer.render_result(result, &ctx) {
                        let fallback =
                            tool_completion_summary(tool_name, result, &MAIN_SUMMARY_FORMAT);
                        app.push_rendered_message(
                            MessageRole::Tool,
                            tool_display_name(tool_name).to_string(),
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
                let summary = tool_completion_summary(tool_name, result, &MAIN_SUMMARY_FORMAT);
                app.push_message(
                    MessageRole::Tool,
                    tool_display_name(tool_name).to_string(),
                    summary,
                    false,
                    None,
                );
            }

            app.status_text = "Ready".into();
            tracing::debug!(call_id = %call_id, tool = %tool_name, is_error = result.is_error, "tool_close");
        },
        EventPayload::Durable(DurableEventPayload::ToolCallFailed {
            call_id,
            tool_name,
            error,
            ..
        }) => {
            close_tool_call_state(app, call_id.as_str());
            app.push_message(
                MessageRole::Error,
                tool_display_name(tool_name).to_string(),
                format!("✗ Execution failed: {}", truncate_first_line(error, 100)),
                false,
                None,
            );
            app.status_text = "Ready".into();
            tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_failed");
        },
        EventPayload::Durable(DurableEventPayload::ToolCallCancelled {
            call_id,
            tool_name,
            reason,
            ..
        }) => {
            close_tool_call_state(app, call_id.as_str());
            app.push_message(
                MessageRole::Tool,
                tool_display_name(tool_name).to_string(),
                format!("○ Cancelled: {}", truncate_first_line(reason, 100)),
                false,
                None,
            );
            app.status_text = "Ready".into();
            tracing::debug!(call_id = %call_id, tool = %tool_name, "tool_cancelled");
        },
        EventPayload::Live(LiveEventPayload::CompactionStarted) => {
            app.is_compacting = true;
            app.push_message(
                MessageRole::System,
                "Compacting".into(),
                "Compacting context...".into(),
                true,
                Some("compaction".into()),
            );
            app.status_text = "Compacting...".into();
        },
        EventPayload::Live(LiveEventPayload::CompactionCompleted { messages_removed }) => {
            app.is_compacting = false;

            // 更新 streaming 消息为完成状态
            if let Some(idx) = app
                .messages
                .iter()
                .position(|m| m.key.as_deref() == Some("compaction"))
            {
                app.messages.remove(idx);
            }

            app.push_message(
                MessageRole::System,
                "Compacted".into(),
                format!("Compacted (removed {} messages)", messages_removed),
                false,
                None,
            );
            app.status_text = "Ready".into();
        },
        EventPayload::Durable(DurableEventPayload::ErrorOccurred { message, .. })
        | EventPayload::Live(LiveEventPayload::ErrorOccurred { message, .. }) => {
            app.show_error(message);
        },
        EventPayload::Durable(DurableEventPayload::RecapGenerated { text, .. }) => {
            app.push_message(
                MessageRole::System,
                "Recap".into(),
                text.clone(),
                false,
                None,
            );
            app.status_text = "Ready".into();
        },
        EventPayload::Durable(DurableEventPayload::ModelIdChanged { model_id }) => {
            app.model_name = model_id.clone();
        },
        EventPayload::Durable(DurableEventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_call_id,
            ..
        }) => {
            let short_task = truncate_first_line(task, 60);
            app.push_message(
                MessageRole::System,
                format!("Agent({agent_name})"),
                short_task,
                false,
                None,
            );
            app.status_text = format!("● Agent: {agent_name}");

            if let Some(tool_call_id) = tool_call_id {
                app.child_session_map
                    .insert(child_session_id.to_string(), tool_call_id.to_string());
            }
        },
        EventPayload::Durable(DurableEventPayload::AgentSessionCompleted {
            child_session_id,
            summary,
            ..
        }) => {
            let was_tracked_child = is_tracked_child(app, child_session_id.as_str());
            let short_summary = truncate_first_line(summary, 60);
            if !was_tracked_child {
                app.push_message(
                    MessageRole::Tool,
                    "Agent".into(),
                    format!("● Done — {short_summary}"),
                    false,
                    None,
                );
            }
            app.child_session_map.remove(child_session_id.as_str());
            app.status_text = "Ready".into();
        },
        EventPayload::Durable(DurableEventPayload::AgentSessionFailed {
            child_session_id,
            error,
            ..
        }) => {
            let was_tracked_child = is_tracked_child(app, child_session_id.as_str());
            if !was_tracked_child {
                app.push_message(
                    MessageRole::Error,
                    "Agent".into(),
                    format!("✗ {}", truncate_first_line(error, 80)),
                    false,
                    None,
                );
            }
            app.child_session_map.remove(child_session_id.as_str());
        },
        _ => {
            if let Some(custom_event) = event.payload.custom_event() {
                apply_custom_event(app, custom_event);
            }
        },
    }
}

fn close_tool_call_state(app: &mut App, call_id: &str) {
    if let Some(index) = app
        .messages
        .iter()
        .rposition(|message| message.key.as_deref() == Some(call_id))
    {
        app.messages.remove(index);
    }
    if let Some(mut tracker) = app.child_agents.remove(call_id) {
        tracker.flush_on_completion(&mut app.scrollback_queue);
    }
    app.child_session_map
        .retain(|_, mapped_call_id| mapped_call_id != call_id);
}

fn is_tracked_child(app: &App, child_session_id: &str) -> bool {
    app.child_session_map
        .get(child_session_id)
        .is_some_and(|call_id| app.child_agents.contains_key(call_id))
}

fn apply_custom_event(app: &mut App, custom_event: &CustomEventData) {
    let name = &custom_event.event_type;
    let fallback = format!(
        "[{name}] {}",
        inline_preview(&custom_event.payload.to_string(), 80)
    );
    let body = MessageBody::with_custom(name.clone(), custom_event.payload.clone(), fallback);
    let message = Message {
        role: MessageRole::System,
        label: name.clone(),
        body,
        is_streaming: false,
        key: None,
    };
    app.scrollback_queue
        .push(ScrollbackEntry::Message(message.clone()));
    app.messages.push(message);
}

/// 处理来自子 session 的事件，将工具调用进度路由到对应的 ChildAgentTracker。
fn apply_child_session_event(app: &mut App, call_id: &str, event: &Event) {
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

/// tool_completion_summary 的格式化参数，区分主会话与子 agent 两种展示风格。
struct ToolSummaryFormat {
    /// 摘要前缀（主会话 "● "，子 agent 无）
    prefix: &'static str,
    /// shell 单行输出的截断长度
    shell_preview_max: usize,
    /// 默认分支单行输出的截断长度
    fallback_preview_max: usize,
    /// shell 无输出时的文案
    shell_empty: &'static str,
    /// 无实质内容时的完成文案
    done: &'static str,
    /// read 分支的动词前缀
    read_verb: &'static str,
    /// glob 分支的动词前缀
    glob_verb: &'static str,
    /// shell 多行输出计数是否加括号
    shell_lines_parens: bool,
}

const MAIN_SUMMARY_FORMAT: ToolSummaryFormat = ToolSummaryFormat {
    prefix: "● ",
    shell_preview_max: 80,
    fallback_preview_max: 60,
    shell_empty: "Ran (no output)",
    done: "Done",
    read_verb: "Read ",
    glob_verb: "Found ",
    shell_lines_parens: true,
};

const CHILD_SUMMARY_FORMAT: ToolSummaryFormat = ToolSummaryFormat {
    prefix: "",
    shell_preview_max: 50,
    fallback_preview_max: 50,
    shell_empty: "done",
    done: "done",
    read_verb: "",
    glob_verb: "",
    shell_lines_parens: false,
};

/// 工具完成的单行摘要；主会话调用点已分流 is_error，错误分支仅子 agent 路径触发。
fn tool_completion_summary(
    tool_name: &str,
    result: &astrcode_core::tool::ToolResult,
    fmt: &ToolSummaryFormat,
) -> String {
    let content = result.content.trim();
    if result.is_error {
        return truncate_first_line(result.error.as_deref().unwrap_or(content), 60);
    }
    match tool_name {
        "shell" => {
            let line_count = content.lines().count();
            if line_count <= 1 && !content.is_empty() {
                format!(
                    "{}{}",
                    fmt.prefix,
                    truncate_first_line(content, fmt.shell_preview_max)
                )
            } else if line_count > 1 {
                if fmt.shell_lines_parens {
                    format!("{}({line_count} lines of output)", fmt.prefix)
                } else {
                    format!("{line_count} lines of output")
                }
            } else {
                format!("{}{}", fmt.prefix, fmt.shell_empty)
            }
        },
        "read" => {
            if content.is_empty() && fmt.read_verb.is_empty() {
                format!("{}{}", fmt.prefix, fmt.done)
            } else {
                format!(
                    "{}{}{} line(s)",
                    fmt.prefix,
                    fmt.read_verb,
                    content.lines().count().max(1)
                )
            }
        },
        "write" | "edit" | "patch" => format!("{}{}", fmt.prefix, fmt.done),
        "glob" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{}{}{count} file(s)", fmt.prefix, fmt.glob_verb)
        },
        "grep" => {
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{}{count} match(es)", fmt.prefix)
        },
        _ => {
            if content.is_empty() {
                format!("{}{}", fmt.prefix, fmt.done)
            } else {
                format!(
                    "{}{}",
                    fmt.prefix,
                    truncate_first_line(content, fmt.fallback_preview_max)
                )
            }
        },
    }
}

fn apply_session_resumed(app: &mut App, session_id: &str, snapshot: &SessionSnapshot) {
    app.active_session_id = Some(session_id.to_string());
    app.working_dir = snapshot.working_dir.clone();
    app.messages.clear();
    app.needs_terminal_reset = true;
    app.pending_assistant_stream_reset = None;
    app.stream_states.clear();
    app.child_agents.clear();
    app.child_session_map.clear();
    // 重置 compacting 状态，防止状态不一致
    app.is_compacting = false;

    for message in &snapshot.messages {
        let role = match message.role {
            astrcode_protocol::wire::MessageRoleDto::System => MessageRole::System,
            astrcode_protocol::wire::MessageRoleDto::User => MessageRole::User,
            astrcode_protocol::wire::MessageRoleDto::Assistant => MessageRole::Assistant,
            astrcode_protocol::wire::MessageRoleDto::Tool => MessageRole::Tool,
        };

        let is_compact_summary = message
            .is_compact_summary
            .unwrap_or_else(|| is_compact_summary_text(&message.content));
        let label = if is_compact_summary {
            "Compacted"
        } else {
            match &role {
                MessageRole::User => "You",
                MessageRole::Assistant => "Astrcode",
                MessageRole::System => "System",
                MessageRole::Tool => "Tool",
                MessageRole::Error => "Error",
            }
        };

        app.push_message(role, label.into(), message.content.clone(), false, None);
    }
    app.status_text = format!("Resumed {}", short_id(session_id));
    tracing::debug!(session_id = %session_id, messages = snapshot.messages.len(), "resume_snapshot");
}

fn apply_session_list(app: &mut App, sessions: &[SessionListItemDto]) {
    use crate::tui::app::SessionEntry;
    app.available_sessions = sessions
        .iter()
        .map(|s| {
            let title = s
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|text| inline_preview(text, 60))
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
    commands: &[ExtensionCommandInfoDto],
    keybindings: &[KeybindingDto],
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
    app.extension_command_names = app
        .extension_commands
        .iter()
        .map(|cmd| cmd.name.clone())
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

fn truncate_first_line(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.chars().count() <= max_chars {
        return first_line.to_owned();
    }

    let mut truncated = first_line.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

fn ready_status(reason: &str) -> String {
    if reason == "stop" {
        "Ready".into()
    } else {
        format!("Ready · {reason}")
    }
}

/// Codex-style one-line tool call summary for the status bar.
fn tool_call_summary(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    let action = tool_display_name(tool_name);
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
        "glob" => {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::{
        event::{
            DurableEvent, DurableEventPayload, EventPayload, LiveEvent, LiveEventPayload,
            StoredEvent,
        },
        tool::ToolResult,
    };
    use astrcode_protocol::{events::MessageDto, wire::MessageRoleDto};

    use super::*;
    use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

    fn make_app() -> App {
        App::new()
    }

    fn apply_payload(app: &mut App, payload: EventPayload) {
        let event = match payload {
            EventPayload::Durable(payload) => StoredEvent::new(
                1,
                DurableEvent::turn("session".into(), "turn".into(), payload),
            )
            .into(),
            EventPayload::Live(payload) => {
                LiveEvent::turn("session".into(), "turn".into(), payload).into()
            },
        };
        apply_event(app, &event);
    }

    fn tool_result(content: &str, is_error: bool) -> ToolResult {
        ToolResult {
            content: content.into(),
            is_error,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    #[test]
    fn first_line_preview_preserves_content_and_limits_characters() {
        for (text, max_chars, expected) in [
            ("hello", 10, "hello"),
            ("first\nsecond", 80, "first"),
            ("0123456789abcdef", 8, "01234567…"),
            ("你好世界abcd", 8, "你好世界abcd"),
            ("hello   world", 80, "hello   world"),
        ] {
            assert_eq!(truncate_first_line(text, max_chars), expected);
        }
    }

    #[test]
    fn assistant_deltas_enter_scrollback_incrementally() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "first line\nsecond".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::AssistantMessageCompleted {
                message_id: "msg-1".into(),
                text: "first line\nsecond".into(),
                reasoning_content: None,
            }),
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].body.plain_text(), "first line\nsecond");
        assert!(matches!(
            app.scrollback_queue.first(),
            Some(ScrollbackEntry::AssistantStreamHeader { message_id })
                if message_id == "msg-1"
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
    fn assistant_retry_reset_reuses_streaming_message() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "stale".into(),
            }),
        );
        app.scrollback_queue
            .push(ScrollbackEntry::AssistantStreamText {
                message_id: "msg-1".into(),
                text: "stale committed line".into(),
            });

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantMessageReset {
                message_id: "msg-1".into(),
            }),
        );

        assert_eq!(app.pending_assistant_stream_reset.as_deref(), Some("msg-1"));
        assert!(
            app.scrollback_queue
                .iter()
                .all(|entry| entry.assistant_message_id() != Some("msg-1"))
        );

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "fresh".into(),
            }),
        );

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].body.plain_text(), "fresh");
        assert!(app.scrollback_queue.iter().any(|entry| matches!(
            entry,
            ScrollbackEntry::AssistantStreamHeader { message_id } if message_id == "msg-1"
        )));
        assert!(!app.scrollback_queue.iter().any(|entry| matches!(
            entry,
            ScrollbackEntry::AssistantStreamText { text, .. } if text.contains("stale")
        )));

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::LlmRetrying {
                status: None,
                attempt: 1,
                max_retries: 2,
                delay_ms: 100,
            }),
        );
        assert_eq!(app.status_text, "Reconnecting · 1/2");

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::LlmRetryRecovered),
        );
        assert_eq!(app.status_text, "Thinking");
    }

    #[test]
    fn completion_statuses_preserve_actionable_reasons_without_scrollback() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AgentRunStarted),
        );
        assert!(app.is_streaming);
        assert_eq!(app.status_text, "Agent running");
        assert!(app.scrollback_queue.is_empty());

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AgentRunCompleted {
                reason: "done".into(),
            }),
        );
        assert!(!app.is_streaming);
        assert!(app.scrollback_queue.is_empty());
        assert_eq!(app.status_text, "Ready · done");

        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            }),
        );
        assert_eq!(app.status_text, "Ready");

        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::AgentRunCompleted {
                reason: "aborted".into(),
            }),
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
    fn resumed_snapshot_prefers_explicit_compact_summary_semantics() {
        let mut app = make_app();
        let snapshot = SessionSnapshot {
            session_id: "session".into(),
            cursor: "1".into(),
            messages: vec![
                MessageDto {
                    role: MessageRoleDto::System,
                    content: "<compact_summary>legacy-looking text</compact_summary>".into(),
                    is_compact_summary: Some(false),
                },
                MessageDto {
                    role: MessageRoleDto::System,
                    content: "summary without a marker".into(),
                    is_compact_summary: Some(true),
                },
                MessageDto {
                    role: MessageRoleDto::System,
                    content: "  <compact_summary>legacy summary</compact_summary>".into(),
                    is_compact_summary: None,
                },
            ],
            model_id: "model".into(),
            working_dir: "/workspace".into(),
            agent_sessions: Vec::new(),
        };

        apply_session_resumed(&mut app, "session", &snapshot);

        assert_eq!(app.messages[0].label, "System");
        assert_eq!(app.messages[1].label, "Compacted");
        assert_eq!(app.messages[2].label, "Compacted");
    }

    #[test]
    fn tool_completion_uses_builtin_and_unknown_fallbacks() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
                result: tool_result("match1\nmatch2\nmatch3", false),
                arguments: String::new(),
                arguments_json: None,
            }),
        );
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].body.plain_text().contains("● 3 match"));

        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                call_id: "call-extension".into(),
                tool_name: "thirdPartyTool".into(),
                result: tool_result("plain extension output", false),
                arguments: String::new(),
                arguments_json: None,
            }),
        );
        assert_eq!(app.messages.len(), 2);
        assert!(
            app.messages[1]
                .body
                .plain_text()
                .contains("plain extension output")
        );
    }

    #[test]
    fn tool_error_shows_in_transcript() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                result: tool_result("permission denied", true),
                arguments: String::new(),
                arguments_json: None,
            }),
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::Error);
        assert!(app.messages[0].body.plain_text().contains("✗"));
    }

    #[test]
    fn tool_execution_terminal_states_are_distinct() {
        let cases = [
            (
                EventPayload::Durable(DurableEventPayload::ToolCallFailed {
                    call_id: "failed".into(),
                    tool_name: "shell".into(),
                    error: "process spawn failed".into(),
                    metadata: Default::default(),
                    duration_ms: None,
                    arguments: String::new(),
                    arguments_json: None,
                }),
                MessageRole::Error,
                "Execution failed",
            ),
            (
                EventPayload::Durable(DurableEventPayload::ToolCallCancelled {
                    call_id: "cancelled".into(),
                    tool_name: "shell".into(),
                    reason: "turn aborted".into(),
                    duration_ms: None,
                    arguments: String::new(),
                    arguments_json: None,
                }),
                MessageRole::Tool,
                "Cancelled",
            ),
        ];

        for (payload, expected_role, expected_text) in cases {
            let mut app = make_app();
            apply_payload(&mut app, payload);
            assert_eq!(app.messages.len(), 1);
            assert_eq!(app.messages[0].role, expected_role);
            assert!(app.messages[0].body.plain_text().contains(expected_text));
        }
    }

    #[test]
    fn agent_tool_shows_compact_task_summary() {
        let mut app = make_app();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::ToolCallStarted {
                call_id: "call-agent".into(),
                tool_name: "agent".into(),
            }),
        );
        apply_payload(
            &mut app,
            EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                call_id: "call-agent".into(),
                tool_name: "agent".into(),
                result: tool_result("Found 3 relevant files", false),
                arguments: String::new(),
                arguments_json: None,
            }),
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
            EventPayload::Live(LiveEventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
            }),
        );
        app.scrollback_queue.clear();
        apply_payload(
            &mut app,
            EventPayload::Live(LiveEventPayload::ToolOutputDelta {
                call_id: "call-1".into(),
                stream: astrcode_core::event::ToolOutputStream::Stdout,
                delta: "lots of output\n".into(),
            }),
        );
        assert!(app.scrollback_queue.is_empty());
        assert!(app.status_text.contains("Receiving"));
    }
}
