//! TUI state backing the transcript and composer surfaces.

use astrcode_core::event::{Event, EventPayload};
use astrcode_protocol::events::ClientNotification;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub label: String,
    pub content: String,
    pub is_streaming: bool,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    SlashPalette,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub messages: Vec<Message>,
    pub is_streaming: bool,
    pub input: String,
    pub input_cursor: usize,
    pub input_history: Vec<String>,
    pub input_history_idx: Option<usize>,
    pub available_sessions: Vec<String>,
    pub active_session_id: Option<String>,
    pub focus: Focus,
    pub show_slash_palette: bool,
    pub slash_filter: String,
    pub slash_selected: usize,
    pub status: String,
    pub error: Option<String>,
    pub model_name: String,
    pub working_dir: String,
    pub dirty: bool,
    pub should_quit: bool,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            is_streaming: false,
            input: String::new(),
            input_cursor: 0,
            input_history: Vec::new(),
            input_history_idx: None,
            available_sessions: Vec::new(),
            active_session_id: None,
            focus: Focus::Input,
            show_slash_palette: false,
            slash_filter: String::new(),
            slash_selected: 0,
            status: "Ready".into(),
            error: None,
            model_name: String::new(),
            working_dir: String::new(),
            dirty: true,
            should_quit: false,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn insert_char(&mut self, ch: char) {
        let byte_idx = self.cursor_byte_index();
        self.input.insert(byte_idx, ch);
        self.input_cursor += 1;
        self.sync_slash_filter();
        self.mark_dirty();
    }

    pub fn insert_newline(&mut self) {
        let byte_idx = self.cursor_byte_index();
        self.input.insert(byte_idx, '\n');
        self.input_cursor += 1;
        self.sync_slash_filter();
        self.mark_dirty();
    }

    pub fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let remove_byte_idx = self
            .input
            .char_indices()
            .nth(self.input_cursor - 1)
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.input.remove(remove_byte_idx);
        self.input_cursor -= 1;
        self.sync_slash_filter();
        self.mark_dirty();
    }

    pub fn delete(&mut self) {
        let char_count = self.input.chars().count();
        if self.input_cursor >= char_count {
            return;
        }
        let remove_byte_idx = self
            .input
            .char_indices()
            .nth(self.input_cursor)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.input.len());
        self.input.remove(remove_byte_idx);
        self.sync_slash_filter();
        self.mark_dirty();
    }

    pub fn move_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
        self.mark_dirty();
    }

    pub fn move_right(&mut self) {
        self.input_cursor = (self.input_cursor + 1).min(self.input.chars().count());
        self.mark_dirty();
    }

    pub fn move_home(&mut self) {
        self.input_cursor = 0;
        self.mark_dirty();
    }

    pub fn move_end(&mut self) {
        self.input_cursor = self.input.chars().count();
        self.mark_dirty();
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.chars().count();
        self.sync_slash_filter();
        self.mark_dirty();
    }

    pub fn take_input(&mut self) -> String {
        self.input_history_idx = None;
        self.close_slash();
        self.input_cursor = 0;
        std::mem::take(&mut self.input)
    }

    pub fn remember_input(&mut self, input: &str) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.input_history.last().map(|v| v.as_str()) != Some(trimmed) {
            self.input_history.push(trimmed.to_string());
        }
    }

    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let next_idx = match self.input_history_idx {
            Some(idx) if idx > 0 => idx - 1,
            Some(idx) => idx,
            None => self.input_history.len().saturating_sub(1),
        };
        self.input_history_idx = Some(next_idx);
        self.set_input(self.input_history[next_idx].clone());
    }

    pub fn history_next(&mut self) {
        let Some(idx) = self.input_history_idx else {
            return;
        };
        if idx + 1 >= self.input_history.len() {
            self.input_history_idx = None;
            self.set_input(String::new());
            return;
        }
        let next_idx = idx + 1;
        self.input_history_idx = Some(next_idx);
        self.set_input(self.input_history[next_idx].clone());
    }

    pub fn close_slash(&mut self) {
        self.show_slash_palette = false;
        self.focus = Focus::Input;
        self.slash_filter.clear();
        self.slash_selected = 0;
        self.mark_dirty();
    }

    pub fn slash_move_up(&mut self, len: usize) {
        if len == 0 {
            self.slash_selected = 0;
        } else if self.slash_selected == 0 {
            self.slash_selected = len - 1;
        } else {
            self.slash_selected -= 1;
        }
        self.mark_dirty();
    }

    pub fn slash_move_down(&mut self, len: usize) {
        if len == 0 {
            self.slash_selected = 0;
        } else {
            self.slash_selected = (self.slash_selected + 1) % len;
        }
        self.mark_dirty();
    }

    pub fn apply(&mut self, notification: &ClientNotification) {
        match notification {
            ClientNotification::Event(event) => self.apply_event(event),
            ClientNotification::SessionResumed {
                session_id,
                snapshot,
            } => {
                self.active_session_id = Some(session_id.clone());
                self.working_dir = snapshot.working_dir.clone();
                self.messages.clear();
                for message in &snapshot.messages {
                    let role = match message.role.as_str() {
                        "user" => MessageRole::User,
                        "assistant" => MessageRole::Assistant,
                        "tool" => MessageRole::Tool,
                        _ => MessageRole::System,
                    };
                    let label = match role {
                        MessageRole::User => "You",
                        MessageRole::Assistant => "Astrcode",
                        MessageRole::System => "System",
                        MessageRole::Tool => "Tool",
                        MessageRole::Error => "Error",
                    };
                    self.push_message(role, label.into(), message.content.clone(), false, None);
                }
                self.status = format!("Resumed {}", super::short_id(session_id));
            },
            ClientNotification::SessionList { sessions } => {
                self.available_sessions = sessions
                    .iter()
                    .map(|item| item.session_id.clone())
                    .collect();
                self.status = format!("{} session(s)", sessions.len());
                self.mark_dirty();
            },
            ClientNotification::UiRequest { message, .. } => {
                self.status = message.clone();
                self.mark_dirty();
            },
            ClientNotification::Error { message, .. } => {
                self.show_error(message);
            },
        }
    }

    fn apply_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::SessionStarted {
                working_dir,
                model_id,
            } => {
                self.active_session_id = Some(event.session_id.clone());
                self.working_dir = working_dir.clone();
                self.model_name = model_id.clone();
                self.push_message(
                    MessageRole::System,
                    "Session".into(),
                    format!("Created session {}", super::short_id(&event.session_id)),
                    false,
                    None,
                );
                self.status = "Ready".into();
            },
            EventPayload::SessionDeleted => {
                self.active_session_id = None;
                self.status = "Session deleted".into();
                self.mark_dirty();
            },
            EventPayload::TurnStarted => {
                self.is_streaming = true;
                self.error = None;
                self.status = "Working".into();
                self.mark_dirty();
            },
            EventPayload::TurnCompleted { finish_reason } => {
                self.is_streaming = false;
                self.status = format!("Ready · {}", finish_reason);
                self.mark_dirty();
            },
            // User messages are pushed optimistically when Enter is pressed.
            EventPayload::UserMessage { .. } => {},
            EventPayload::AssistantMessageStarted { message_id } => {
                self.push_message(
                    MessageRole::Assistant,
                    "Astrcode".into(),
                    String::new(),
                    true,
                    Some(message_id.clone()),
                );
            },
            EventPayload::AssistantTextDelta { message_id, delta } => {
                if let Some(message) = self.find_message_mut(message_id) {
                    message.content.push_str(delta);
                    self.mark_dirty();
                }
            },
            EventPayload::AssistantMessageCompleted { message_id, text } => {
                if let Some(message) = self.find_message_mut(message_id) {
                    message.content = text.clone();
                    message.is_streaming = false;
                    self.mark_dirty();
                } else {
                    self.push_message(
                        MessageRole::Assistant,
                        "Astrcode".into(),
                        text.clone(),
                        false,
                        Some(message_id.clone()),
                    );
                }
            },
            EventPayload::ThinkingDelta { delta } => {
                self.status = format!("Thinking · {}", delta);
                self.mark_dirty();
            },
            EventPayload::ToolCallStarted { call_id, tool_name } => {
                if !should_print_tool(tool_name) {
                    self.status = format!("Running {}", tool_name);
                    self.mark_dirty();
                    return;
                }
                self.push_message(
                    MessageRole::Tool,
                    format!("Tool · {}", tool_name),
                    tool_name.clone(),
                    true,
                    Some(call_id.clone()),
                );
            },
            EventPayload::ToolCallArgumentsDelta { call_id, delta } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    if !message.content.ends_with('\n') {
                        message.content.push('\n');
                    }
                    message.content.push_str(delta);
                    self.mark_dirty();
                }
            },
            EventPayload::ToolCallRequested {
                call_id,
                tool_name,
                arguments,
            } => {
                if !should_print_tool(tool_name) {
                    self.status = format!("Running {}", tool_name);
                    self.mark_dirty();
                    return;
                }
                let args = serde_json::to_string(arguments).unwrap_or_default();
                let body = if args.is_empty() || args == "{}" {
                    tool_name.clone()
                } else {
                    format!("{tool_name}\n{args}")
                };
                if let Some(message) = self.find_message_mut(call_id) {
                    message.content = body;
                    self.mark_dirty();
                } else {
                    self.push_message(
                        MessageRole::Tool,
                        format!("Tool · {}", tool_name),
                        body,
                        true,
                        Some(call_id.clone()),
                    );
                }
            },
            EventPayload::ToolOutputDelta { call_id, delta, .. } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    if !message.content.is_empty() {
                        message.content.push('\n');
                    }
                    message.content.push_str(delta);
                    self.mark_dirty();
                }
            },
            EventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
            } => {
                if !should_print_tool(tool_name) && !result.is_error {
                    self.status = format!("{} completed", tool_name);
                    self.mark_dirty();
                    return;
                }

                if let Some(message) = self.find_message_mut(call_id) {
                    if !result.content.is_empty() && !message.content.contains(&result.content) {
                        if !message.content.is_empty() {
                            message.content.push('\n');
                        }
                        message.content.push_str(&result.content);
                    }
                    if result.is_error {
                        message.role = MessageRole::Error;
                        message.label = "Tool Error".into();
                    }
                    message.is_streaming = false;
                    self.mark_dirty();
                } else if result.is_error {
                    self.push_message(
                        MessageRole::Error,
                        "Tool Error".into(),
                        result
                            .error
                            .clone()
                            .unwrap_or_else(|| result.content.clone()),
                        false,
                        Some(call_id.clone()),
                    );
                }
            },
            EventPayload::CompactionStarted => {
                self.push_message(
                    MessageRole::System,
                    "System".into(),
                    "Compacting context...".into(),
                    true,
                    Some("compaction".into()),
                );
            },
            EventPayload::CompactionCompleted {
                pre_tokens,
                post_tokens,
                ..
            } => {
                if let Some(message) = self.find_message_mut("compaction") {
                    message.content = format!(
                        "Compaction finished: {} -> {} tokens",
                        pre_tokens, post_tokens
                    );
                    message.is_streaming = false;
                }
                self.status = "Ready".into();
                self.mark_dirty();
            },
            EventPayload::AgentRunStarted => {
                self.is_streaming = true;
                self.status = "Agent running".into();
                self.mark_dirty();
            },
            EventPayload::AgentRunCompleted { reason } => {
                self.is_streaming = false;
                self.status = format!("Ready · {}", reason);
                self.mark_dirty();
            },
            EventPayload::ErrorOccurred { message, .. } => {
                self.show_error(message);
            },
            EventPayload::Custom { name, .. } => {
                self.status = format!("Event: {name}");
                self.mark_dirty();
            },
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.push_message(MessageRole::User, "You".into(), text.into(), false, None);
    }

    fn show_error(&mut self, message: &str) {
        self.error = Some(message.into());
        self.is_streaming = false;
        self.push_message(
            MessageRole::Error,
            "Error".into(),
            message.into(),
            false,
            None,
        );
        self.status = "Error".into();
    }

    pub(crate) fn push_message(
        &mut self,
        role: MessageRole,
        label: String,
        content: String,
        is_streaming: bool,
        key: Option<String>,
    ) {
        self.messages.push(Message {
            role,
            label,
            content,
            is_streaming,
            key,
        });
        self.mark_dirty();
    }

    fn find_message_mut(&mut self, key: &str) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .rev()
            .find(|message| message.key.as_deref() == Some(key))
    }

    fn sync_slash_filter(&mut self) {
        if self.input.starts_with('/') {
            self.show_slash_palette = true;
            self.focus = Focus::SlashPalette;
            self.slash_filter = self
                .input
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
        } else if self.focus == Focus::SlashPalette {
            self.close_slash();
        }
    }

    fn cursor_byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.input_cursor)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.input.len())
    }
}


fn should_print_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell" | "editFile" | "apply_patch" | "applyPatch"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::{
        event::{Event, EventPayload},
        tool::ToolResult,
    };

    use super::*;

    fn apply_payload(state: &mut TuiState, payload: EventPayload) {
        let event = Event::new("session".into(), Some("turn".into()), payload);
        state.apply_event(&event);
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
    fn search_tool_results_do_not_enter_transcript() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "grep".into(),
                result: tool_result("large search output", false),
            },
        );

        assert!(state.messages.is_empty());
        assert_eq!(state.status, "grep completed");
    }

    #[test]
    fn shell_tool_results_still_enter_transcript() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
            EventPayload::ToolCallStarted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                result: tool_result("command output", false),
            },
        );

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::Tool);
        assert!(state.messages[0].content.contains("command output"));
    }

    #[test]
    fn hidden_tool_errors_still_enter_transcript() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
            EventPayload::ToolCallCompleted {
                call_id: "call-1".into(),
                tool_name: "findFiles".into(),
                result: tool_result("glob failed", true),
            },
        );

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::Error);
        assert_eq!(state.messages[0].content, "glob failed");
    }
}
