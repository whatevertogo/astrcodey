//! App: main state machine.
//!
//! Owns session state, component tree, extension registries, and streaming state.
//! Phase 7 will implement the full apply() and dispatch_key() logic.

pub mod handle_event;

use std::collections::BTreeMap;

use astrcode_core::{render::RenderSpec, storage::AgentSessionStatus};
use astrcode_protocol::events::{
    ClientNotification, ExtensionCommandInfo, SessionListItem, SessionSnapshot,
};

use crate::tui_v2::{
    command::slash::SlashCommandSpec,
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
    // Transcript
    pub messages: Vec<Message>,
    pub scrollback_queue: Vec<ScrollbackEntry>,
    // Streaming state
    pub stream_states: BTreeMap<String, crate::tui_v2::streaming::controller::StreamController>,
    pub child_agents: BTreeMap<String, crate::tui_v2::store::child_agent::ChildAgentTracker>,
    // Extension registries
    pub tool_renderers: ToolRendererRegistry,
    pub message_renderers: MessageRendererRegistry,
    // Theme
    pub theme: Theme,
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
}
