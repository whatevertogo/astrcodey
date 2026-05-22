//! App: main state machine.
//!
//! Owns session state, component tree, extension registries, and streaming state.

pub mod handle_event;

use std::collections::BTreeMap;

use astrcode_core::render::RenderSpec;
use astrcode_protocol::events::ClientNotification;

use crate::tui::{
    command::slash::{self, SlashCommandSpec},
    composer::ComposerState,
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
    pub available_sessions: Vec<SessionEntry>,
    // UI state
    pub status_text: String,
    pub error: Option<String>,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub should_quit: bool,
    /// Ctrl+C 二次确认：首次按下后等待第二次确认退出。
    pub quit_pending: bool,
    pub extension_commands: Vec<SlashCommandSpec>,
    /// 插件注册的状态栏项（由 StatusItemUpdate 通知驱动）。
    pub status_items: BTreeMap<String, String>,
    /// 插件注册的快捷键绑定（启动时从服务端获取）。
    pub keybindings: Vec<crate::tui::keybinding::RegisteredKeybinding>,
    /// 服务端扩展注册表变化后，主循环应重新拉取扩展命令快照。
    pub needs_extension_refresh: bool,
    /// Resume / 切换会话后需要清屏重置终端。
    pub needs_terminal_reset: bool,
    /// 服务端 UI 选择请求。
    pub ui_picker: Option<UiPicker>,
    // Session picker（/resume 触发的选择模式）
    pub session_picker: Option<SessionPicker>,
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
    /// child_session_id → tool_call_id 映射，用于将子 session 事件路由到对应的 tracker。
    pub child_session_map: BTreeMap<String, String>,
    // Extension registries
    pub tool_renderers: ToolRendererRegistry,
    pub message_renderers: MessageRendererRegistry,
    // Theme
    pub theme: Theme,
}

/// 会话列表中的一条会话。
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub title: String,
    pub working_dir: String,
    pub is_child: bool,
    /// ISO 8601 格式的最后活跃时间，用于排序。
    pub last_active_at: String,
}

/// Session picker 交互状态（/resume 触发）。
#[derive(Debug, Clone)]
pub struct SessionPicker {
    pub items: Vec<SessionEntry>,
    pub selected: usize,
    /// 用于过滤的规范化 cwd，同时作为 picker 顶部展示的目录提示。
    pub cwd: String,
}

/// 服务端发起的通用选择器。
#[derive(Debug, Clone)]
pub struct UiPicker {
    pub request_id: String,
    pub message: String,
    pub items: Vec<String>,
    pub selected: usize,
}

impl App {
    pub fn new(theme: Theme) -> Self {
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
            is_compacting: false,
            should_quit: false,
            quit_pending: false,
            extension_commands: Vec::new(),
            status_items: BTreeMap::new(),
            keybindings: Vec::new(),
            needs_extension_refresh: false,
            needs_terminal_reset: false,
            ui_picker: None,
            session_picker: None,
            composer: ComposerState::default(),
            show_slash_palette: false,
            slash_filter: String::new(),
            slash_selected: 0,
            messages: Vec::new(),
            scrollback_queue: Vec::new(),
            stream_states: BTreeMap::new(),
            child_agents: BTreeMap::new(),
            child_session_map: BTreeMap::new(),
            tool_renderers,
            message_renderers,
            theme,
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

    pub fn push_rendered_message(
        &mut self,
        role: MessageRole,
        label: String,
        spec: RenderSpec,
        fallback_text: String,
        is_streaming: bool,
        key: Option<String>,
    ) {
        let mut body = MessageBody::text(String::new());
        body.set_render(spec, fallback_text);
        let msg = Message {
            role,
            label,
            body,
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

    /// Ctrl+C 二次确认退出。首次按下显示提示，第二次确认退出。
    pub fn handle_quit_request(&mut self) {
        if self.quit_pending {
            self.should_quit = true;
        } else {
            self.quit_pending = true;
            self.status_text = "Press Ctrl+C again to quit".into();
        }
    }

    /// 重置退出等待状态（任何非 Ctrl+C 的按键都应调用）。
    pub fn reset_quit_pending(&mut self) {
        if self.quit_pending {
            self.quit_pending = false;
            self.status_text = "Ready".into();
        }
    }

    pub fn resolve_session_id(&self, input: &str) -> String {
        let needle = input.trim();
        self.available_sessions
            .iter()
            .find(|s| s.session_id.starts_with(needle))
            .map(|s| s.session_id.clone())
            .unwrap_or_else(|| needle.to_string())
    }

    /// 打开 session picker：筛选当前 cwd 的 session（排除子会话和当前活跃 session），
    /// 按最后活跃时间倒序排列，最多显示 10 个。
    pub fn open_session_picker(&mut self) {
        // 优先使用进程 cwd（尚无活跃 session 时 self.working_dir 为空）
        let raw_cwd = if self.working_dir.is_empty() {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        } else {
            self.working_dir.clone()
        };
        let cwd = crate::tui::store::session_picker::canonicalize_working_dir(&raw_cwd);
        let active = self.active_session_id.as_deref();
        let mut items: Vec<SessionEntry> = self
            .available_sessions
            .iter()
            .filter(|s| {
                if s.is_child {
                    return false;
                }
                if active.is_some_and(|a| s.session_id == a) {
                    return false;
                }
                let s_cwd =
                    crate::tui::store::session_picker::canonicalize_working_dir(&s.working_dir);
                s_cwd == cwd
            })
            .cloned()
            .collect();
        // 按 last_active_at 倒序（最近的在前）
        items.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
        // 限制为最近 10 个
        items.truncate(10);
        self.session_picker = Some(SessionPicker {
            items,
            selected: 0,
            cwd,
        });
    }

    pub fn close_session_picker(&mut self) {
        self.session_picker = None;
    }

    pub fn session_picker_up(&mut self) {
        if let Some(picker) = &mut self.session_picker {
            if picker.selected > 0 {
                picker.selected -= 1;
            }
        }
    }

    pub fn session_picker_down(&mut self) {
        if let Some(picker) = &mut self.session_picker {
            if picker.selected + 1 < picker.items.len() {
                picker.selected += 1;
            }
        }
    }

    pub fn session_picker_accept(&mut self) -> Option<String> {
        let picker = self.session_picker.take()?;
        picker
            .items
            .get(picker.selected)
            .map(|s| s.session_id.clone())
    }

    pub fn open_ui_picker(&mut self, request_id: String, message: String, items: Vec<String>) {
        self.status_text = message.clone();
        self.ui_picker = Some(UiPicker {
            request_id,
            message,
            items,
            selected: 0,
        });
    }

    pub fn close_ui_picker(&mut self) {
        self.ui_picker = None;
    }

    pub fn ui_picker_up(&mut self) {
        if let Some(picker) = &mut self.ui_picker {
            if picker.selected > 0 {
                picker.selected -= 1;
            }
        }
    }

    pub fn ui_picker_down(&mut self) {
        if let Some(picker) = &mut self.ui_picker {
            if picker.selected + 1 < picker.items.len() {
                picker.selected += 1;
            }
        }
    }

    pub fn ui_picker_accept(&mut self) -> Option<(String, String)> {
        let picker = self.ui_picker.take()?;
        picker
            .items
            .get(picker.selected)
            .map(|selected| (picker.request_id, selected.clone()))
    }
}
