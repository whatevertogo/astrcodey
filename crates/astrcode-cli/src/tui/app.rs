//! App: main state machine.
//!
//! Owns session state, component tree, extension registries, and streaming state.

pub mod handle_event;

use std::collections::BTreeMap;

use astrcode_protocol::events::ClientNotification;

use crate::tui::{
    command::slash::{self, SlashCommandSpec},
    component::composer::ComposerState,
    ext::{
        builtin::register_builtin, fallback::DefaultToolRenderer, message::MessageRendererRegistry,
        tool::ToolRendererRegistry,
    },
    store::transcript::{Message, MessageBody, MessageRole, ScrollbackEntry},
    theme::Theme,
};

pub struct App {
    // Session state
    pub active_session_id: Option<String>,
    pub working_dir: String,
    pub model_name: String,
    pub available_sessions: Vec<String>,
    // UI state
    pub status_text: String,
    pub error: Option<String>,
    pub is_streaming: bool,
    pub should_quit: bool,
    pub extension_commands: Vec<SlashCommandSpec>,
    // Mode — plan mode 或 code mode
    pub mode: AppMode,
    // Composer
    pub composer: ComposerState,
    pub show_slash_palette: bool,
    pub slash_filter: String,
    pub slash_selected: usize,
    // Transcript
    pub messages: Vec<Message>,
    pub scrollback_queue: Vec<ScrollbackEntry>,
    // Streaming state
    pub stream_states: BTreeMap<String, crate::tui::streaming::controller::StreamController>,
    pub child_agents: BTreeMap<String, crate::tui::store::child_agent::ChildAgentTracker>,
    // Extension registries
    pub tool_renderers: ToolRendererRegistry,
    pub message_renderers: MessageRendererRegistry,
    // Theme
    pub theme: Theme,
}

/// 工作模式：决定 agent 的行为方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Code 模式 — agent 直接执行代码修改（默认）。
    Code,
    /// Plan 模式 — agent 仅输出计划，不执行工具。
    Plan,
}

impl App {
    pub fn new() -> Self {
        let fallback = std::sync::Arc::new(DefaultToolRenderer);
        let mut tool_renderers = ToolRendererRegistry::new(fallback);
        let mut message_renderers = MessageRendererRegistry::new();
        register_builtin(&mut tool_renderers, &mut message_renderers);

        Self {
            active_session_id: None,
            working_dir: String::new(),
            model_name: String::new(),
            available_sessions: Vec::new(),
            status_text: "Ready".into(),
            error: None,
            is_streaming: false,
            should_quit: false,
            extension_commands: Vec::new(),
            mode: AppMode::Code,
            composer: ComposerState::default(),
            show_slash_palette: false,
            slash_filter: String::new(),
            slash_selected: 0,
            messages: Vec::new(),
            scrollback_queue: Vec::new(),
            stream_states: BTreeMap::new(),
            child_agents: BTreeMap::new(),
            tool_renderers,
            message_renderers,
            theme: Theme::detect(),
        }
    }

    pub fn apply(&mut self, notification: &ClientNotification) {
        handle_event::apply(self, notification);
    }

    // ─── Composer helpers ─────────────────────────────────────────────────────

    pub fn input_text(&self) -> &str {
        self.composer.text()
    }

    pub fn input_cursor(&self) -> usize {
        self.composer.cursor()
    }

    pub fn take_input(&mut self) -> String {
        self.close_slash();
        self.composer.take_submit_text()
    }

    pub fn set_input(&mut self, text: String) {
        self.composer.set_text(text);
        self.sync_slash_filter();
    }

    pub fn remember_input(&mut self, input: &str) {
        self.composer.remember_input(input);
    }

    pub fn history_previous(&mut self) {
        if self.composer.history_previous() {
            self.sync_slash_filter();
        }
    }

    pub fn history_next(&mut self) {
        if self.composer.history_next() {
            self.sync_slash_filter();
        }
    }

    pub fn close_slash(&mut self) {
        self.show_slash_palette = false;
        self.slash_filter.clear();
        self.slash_selected = 0;
    }

    pub fn slash_move_up(&mut self) {
        let len = slash::filtered(&self.slash_filter, &self.extension_commands).len();
        if len == 0 {
            self.slash_selected = 0;
        } else if self.slash_selected == 0 {
            self.slash_selected = len - 1;
        } else {
            self.slash_selected -= 1;
        }
    }

    pub fn slash_move_down(&mut self) {
        let len = slash::filtered(&self.slash_filter, &self.extension_commands).len();
        if len == 0 {
            self.slash_selected = 0;
        } else {
            self.slash_selected = (self.slash_selected + 1) % len;
        }
    }

    fn sync_slash_filter(&mut self) {
        let input = self.composer.text().to_string();
        if input.starts_with('/') {
            let filter = input
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            self.show_slash_palette = true;
            self.slash_filter = filter;
        } else if self.show_slash_palette {
            self.close_slash();
        }
    }

    /// Public alias for use from the main loop (mod.rs).
    pub fn sync_slash_filter_pub(&mut self) {
        self.sync_slash_filter();
    }

    // ─── Transcript helpers ───────────────────────────────────────────────────
    pub fn push_message(
        &mut self,
        role: MessageRole,
        label: String,
        content: String,
        is_streaming: bool,
        key: Option<String>,
    ) {
        let msg = Message {
            role,
            label,
            body: MessageBody::text(content),
            is_streaming,
            key,
        };
        if !is_streaming {
            self.scrollback_queue
                .push(ScrollbackEntry::Message(msg.clone()));
        }
        self.messages.push(msg);
    }

    pub fn push_user(&mut self, text: &str) {
        self.push_message(MessageRole::User, "You".into(), text.into(), false, None);
    }

    pub fn find_message_mut(&mut self, key: &str) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .rev()
            .find(|m| m.key.as_deref() == Some(key))
    }

    pub fn show_error(&mut self, message: &str) {
        self.error = Some(message.into());
        self.is_streaming = false;
        self.push_message(
            MessageRole::Error,
            "Error".into(),
            message.into(),
            false,
            None,
        );
        self.status_text = "Error".into();
    }

    /// 切换工作模式（Code ↔ Plan）。
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            AppMode::Code => AppMode::Plan,
            AppMode::Plan => AppMode::Code,
        };
        let label = match self.mode {
            AppMode::Code => "code",
            AppMode::Plan => "plan",
        };
        self.status_text = format!("Mode: {label}");
        self.push_message(
            MessageRole::System,
            "Mode".into(),
            format!("Switched to {label} mode"),
            false,
            None,
        );
    }

    /// 设置工作模式。
    pub fn set_mode(&mut self, mode: AppMode) {
        if self.mode != mode {
            self.mode = mode;
            let label = match self.mode {
                AppMode::Code => "code",
                AppMode::Plan => "plan",
            };
            self.status_text = format!("Mode: {label}");
            self.push_message(
                MessageRole::System,
                "Mode".into(),
                format!("Switched to {label} mode"),
                false,
                None,
            );
        }
    }

    pub fn resolve_session_id(&self, input: &str) -> String {
        let needle = input.trim();
        self.available_sessions
            .iter()
            .find(|id| id.starts_with(needle))
            .cloned()
            .unwrap_or_else(|| needle.to_string())
    }
}
