//! TUI 状态管理 —— 消息记录和输入编辑器的状态模型。
//!
//! 维护消息列表、输入框内容与光标、会话信息、斜杠命令面板状态等，
//! 并提供将服务器事件（ClientNotification）应用到 UI 状态的方法。

use std::collections::BTreeMap;

use astrcode_core::{
    event::{Event, EventPayload},
    render::{RenderSpec, UI_RENDER_METADATA_KEY},
};
use astrcode_protocol::events::ClientNotification;

use super::{
    composer::{ComposerAction, ComposerState},
    tool_display,
};

/// 消息角色枚举，用于区分不同来源的消息并应用对应样式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    /// 用户发送的消息
    User,
    /// 助手回复的消息
    Assistant,
    /// 工具调用及结果
    Tool,
    /// 系统通知消息
    System,
    /// 错误消息
    Error,
}

/// 消息正文，始终保留纯文本视图，可选携带结构化渲染描述。
#[derive(Debug, Clone)]
pub struct MessageBody {
    plain: String,
    render: Option<RenderSpec>,
}

impl MessageBody {
    fn text(text: String) -> Self {
        Self {
            plain: text,
            render: None,
        }
    }

    /// 取得纯文本视图，用于兼容旧路径、搜索和回退显示。
    pub fn plain_text(&self) -> &str {
        &self.plain
    }

    /// 取得结构化渲染视图，由 TUI 皮肤决定具体展示方式。
    pub fn render_spec(&self) -> Option<&RenderSpec> {
        self.render.as_ref()
    }

    fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }

    fn contains_text(&self, text: &str) -> bool {
        self.plain.contains(text)
    }

    fn set_text(&mut self, text: String) {
        self.plain = text;
        self.render = None;
    }

    fn append_text(&mut self, text: &str) {
        self.plain.push_str(text);
    }

    fn set_render(&mut self, spec: RenderSpec, fallback: String) {
        self.plain = if fallback.is_empty() {
            spec.plain_text_fallback()
        } else {
            fallback
        };
        self.render = Some(spec);
    }
}

/// 单条消息，包含角色标签、正文内容和流式状态。
#[derive(Debug, Clone)]
pub struct Message {
    /// 消息角色，决定渲染样式
    pub role: MessageRole,
    /// 显示标签（如 "You"、"Astrcode"、"Tool"）
    pub label: String,
    /// 消息正文内容
    pub body: MessageBody,
    /// 是否正在流式接收中
    pub is_streaming: bool,
    /// 消息唯一标识，用于流式更新时定位已有消息
    pub key: Option<String>,
}

/// 待写入终端原生 scrollback 的条目。
#[derive(Debug, Clone)]
pub enum ScrollbackEntry {
    /// 完整消息，适用于用户、工具摘要、系统消息和非流式回退。
    Message(Message),
    /// 流式消息的头部，只打印一次。
    StreamHeader { role: MessageRole, label: String },
    /// 流式正文片段，不重复打印角色标签。
    StreamText { role: MessageRole, text: String },
    /// 流式消息结束后的空行。
    BlankLine,
}

/// 焦点位置枚举，指示当前激活的 UI 区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// 输入编辑器获得焦点
    Input,
    /// 斜杠命令面板获得焦点
    SlashPalette,
}

/// TUI 全局状态结构体。
///
/// 持有消息记录、输入框状态、会话信息、UI 标志等所有渲染所需数据。
/// 通过 `dirty` 标志实现按需重绘。
#[derive(Debug, Clone)]
pub struct TuiState {
    /// 消息记录列表
    pub messages: Vec<Message>,
    /// 消息记录区距离底部的滚动行数
    pub transcript_scroll: usize,
    /// 是否正在流式接收助手回复
    pub is_streaming: bool,
    /// 输入框编辑器状态
    composer: ComposerState,
    /// 可用会话列表
    pub available_sessions: Vec<String>,
    /// 当前活跃会话 ID
    pub active_session_id: Option<String>,
    /// 当前焦点区域
    pub focus: Focus,
    /// 是否显示斜杠命令面板
    pub show_slash_palette: bool,
    /// 斜杠命令过滤字符串
    pub slash_filter: String,
    /// 斜杠命令面板中当前选中项索引
    pub slash_selected: usize,
    /// 状态栏文本
    pub status: String,
    /// 最近一次错误信息
    pub error: Option<String>,
    /// 当前使用的模型名称
    pub model_name: String,
    /// 当前工作目录
    pub working_dir: String,
    /// 脏标记：状态变更后需要重绘
    pub dirty: bool,
    /// 是否应退出 TUI
    pub should_quit: bool,
    /// 待写入 scrollback 的消息队列。
    pub scrollback_queue: Vec<ScrollbackEntry>,
    /// 正在按片段写入 scrollback 的助手消息。
    stream_scrollback: BTreeMap<String, StreamScrollbackState>,
}

#[derive(Debug, Clone, Default)]
struct StreamScrollbackState {
    pending: String,
    seen_delta: bool,
}

impl StreamScrollbackState {
    fn take_ready_chunks(&mut self) -> Vec<String> {
        let mut chunks = Vec::new();
        loop {
            if let Some(newline_idx) = self.pending.find('\n') {
                let chunk = self.pending[..newline_idx].to_string();
                self.pending.drain(..=newline_idx);
                chunks.push(chunk);
                continue;
            }

            if self.pending.chars().count() >= STREAM_CHUNK_CHARS {
                chunks.push(drain_char_prefix(&mut self.pending, STREAM_CHUNK_CHARS));
                continue;
            }

            break;
        }
        chunks
    }
}

const STREAM_CHUNK_CHARS: usize = 160;

fn drain_char_prefix(text: &mut String, char_count: usize) -> String {
    let end = text
        .char_indices()
        .nth(char_count)
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| text.len());
    text.drain(..end).collect()
}

impl TuiState {
    /// 创建初始 TUI 状态，所有字段设为默认值。
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            transcript_scroll: 0,
            is_streaming: false,
            composer: ComposerState::default(),
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
            scrollback_queue: Vec::new(),
            stream_scrollback: BTreeMap::new(),
        }
    }

    /// 标记状态为脏，触发下一帧重绘。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 当前输入框展示文本。
    pub fn input_text(&self) -> &str {
        self.composer.text()
    }

    /// 当前输入框光标位置（char 索引）。
    pub fn input_cursor(&self) -> usize {
        self.composer.cursor()
    }

    /// 在光标位置插入一个字符。
    pub fn insert_char(&mut self, ch: char) {
        self.apply_composer_action(ComposerAction::InsertChar(ch));
    }

    /// 在光标位置插入换行符。
    pub fn insert_newline(&mut self) {
        self.apply_composer_action(ComposerAction::Newline);
    }

    /// 在光标位置插入 bracketed paste 文本；长文本会先折叠为占位符。
    pub fn insert_paste(&mut self, text: &str) {
        self.apply_composer_action(ComposerAction::InsertPaste(text.to_string()));
    }

    /// 删除光标前一个字符（退格键）。
    pub fn backspace(&mut self) {
        self.apply_composer_action(ComposerAction::Backspace);
    }

    /// 删除光标位置的字符（Delete 键）。
    pub fn delete(&mut self) {
        self.apply_composer_action(ComposerAction::Delete);
    }

    /// 光标左移一个字符。
    pub fn move_left(&mut self) {
        self.apply_composer_action(ComposerAction::MoveLeft);
    }

    /// 光标右移一个字符，不超过文本末尾。
    pub fn move_right(&mut self) {
        self.apply_composer_action(ComposerAction::MoveRight);
    }

    /// 光标移动到当前物理行开头。
    pub fn move_home(&mut self) {
        self.apply_composer_action(ComposerAction::MoveHome);
    }

    /// 光标移动到当前物理行末尾。
    pub fn move_end(&mut self) {
        self.apply_composer_action(ComposerAction::MoveEnd);
    }

    /// 光标上移一个视觉行；返回 false 表示已在首行，可交给历史导航。
    pub fn move_visual_up(&mut self, width: usize) -> bool {
        self.apply_composer_action(ComposerAction::MoveVisualUp { width })
    }

    /// 光标下移一个视觉行；返回 false 表示已在末行，可交给历史导航。
    pub fn move_visual_down(&mut self, width: usize) -> bool {
        self.apply_composer_action(ComposerAction::MoveVisualDown { width })
    }

    /// 删除当前行中光标前的内容。
    pub fn delete_before_cursor(&mut self) {
        self.apply_composer_action(ComposerAction::DeleteBeforeCursor);
    }

    /// 删除当前行中光标后的内容。
    pub fn delete_after_cursor(&mut self) {
        self.apply_composer_action(ComposerAction::DeleteAfterCursor);
    }

    /// 删除光标前一个词。
    pub fn delete_previous_word(&mut self) {
        self.apply_composer_action(ComposerAction::DeletePreviousWord);
    }

    /// 替换输入框内容并将光标移到末尾。
    pub fn set_input(&mut self, input: String) {
        self.composer.set_text(input);
        self.sync_slash_filter();
        self.mark_dirty();
    }

    /// 取出输入框内容，清空输入并重置相关状态。
    ///
    /// 返回被取出的文本内容。
    pub fn take_input(&mut self) -> String {
        self.close_slash();
        self.composer.take_submit_text()
    }

    /// 将输入记录到历史列表（去重，不记录空输入）。
    pub fn remember_input(&mut self, input: &str) {
        self.composer.remember_input(input);
    }

    /// 浏览上一条历史输入。
    pub fn history_previous(&mut self) {
        if self.composer.history_previous() {
            self.sync_slash_filter();
            self.mark_dirty();
        }
    }

    /// 浏览下一条历史输入，到达末尾后清空输入框。
    pub fn history_next(&mut self) {
        if self.composer.history_next() {
            self.sync_slash_filter();
            self.mark_dirty();
        }
    }

    fn apply_composer_action(&mut self, action: ComposerAction) -> bool {
        let changed = self.composer.apply(action);
        if changed {
            self.sync_slash_filter();
            self.mark_dirty();
        }
        changed
    }

    /// 关闭斜杠命令面板，恢复焦点到输入框。
    pub fn close_slash(&mut self) {
        self.show_slash_palette = false;
        self.focus = Focus::Input;
        self.slash_filter.clear();
        self.slash_selected = 0;
        self.mark_dirty();
    }

    /// 斜杠面板中上移选中项（循环）。
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

    /// 斜杠面板中下移选中项（循环）。
    pub fn slash_move_down(&mut self, len: usize) {
        if len == 0 {
            self.slash_selected = 0;
        } else {
            self.slash_selected = (self.slash_selected + 1) % len;
        }
        self.mark_dirty();
    }

    /// 回到底部，跟随最新消息。
    pub fn scroll_transcript_to_bottom(&mut self) {
        if self.transcript_scroll != 0 {
            self.transcript_scroll = 0;
            self.mark_dirty();
        }
    }

    /// 将服务器通知应用到 TUI 状态。
    ///
    /// 根据通知类型更新会话信息、消息列表、状态栏等。
    pub fn apply(&mut self, notification: &ClientNotification) {
        match notification {
            ClientNotification::Event(event) => self.apply_event(event),
            // 会话恢复：加载快照中的消息历史
            ClientNotification::SessionResumed {
                session_id,
                snapshot,
            } => {
                self.active_session_id = Some(session_id.clone());
                self.working_dir = snapshot.working_dir.clone();
                self.messages.clear();
                self.stream_scrollback.clear();
                self.transcript_scroll = 0;
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
            // 会话列表更新
            ClientNotification::SessionList { sessions } => {
                self.available_sessions = sessions
                    .iter()
                    .map(|item| item.session_id.clone())
                    .collect();
                self.status = format!("{} session(s)", sessions.len());
                self.push_message(
                    MessageRole::System,
                    "Sessions".into(),
                    session_list_body(sessions, self.active_session_id.as_deref()),
                    false,
                    None,
                );
                self.mark_dirty();
            },
            // UI 请求（如确认提示等）
            ClientNotification::UiRequest { message, .. } => {
                self.status = message.clone();
                self.mark_dirty();
            },
            ClientNotification::Error { message, .. } => {
                self.show_error(message);
            },
        }
    }

    /// 将核心事件（EventPayload）应用到 TUI 状态。
    ///
    /// 处理会话生命周期、对话轮次、助手消息流式更新、
    /// 工具调用、上下文压缩、代理运行等事件。
    fn apply_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::SessionStarted {
                working_dir,
                model_id,
                ..
            } => {
                self.active_session_id = Some(event.session_id.clone());
                self.working_dir = working_dir.clone();
                self.model_name = model_id.clone();
                self.stream_scrollback.clear();
                self.push_message(
                    MessageRole::System,
                    "Session".into(),
                    format!("Created session {}", super::short_id(&event.session_id)),
                    false,
                    None,
                );
                self.status = "Ready".into();
            },
            EventPayload::SystemPromptConfigured { .. } => {
                // Session context fact only; do not render the full system prompt in transcript.
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
            // 用户消息在按下 Enter 时已乐观推入，此处无需处理
            EventPayload::UserMessage { .. } => {},
            EventPayload::AssistantMessageStarted { message_id } => {
                self.stream_scrollback
                    .insert(message_id.clone(), StreamScrollbackState::default());
                self.scrollback_queue.push(ScrollbackEntry::StreamHeader {
                    role: MessageRole::Assistant,
                    label: "Astrcode".into(),
                });
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
                    message.body.append_text(delta);
                    self.mark_dirty();
                }
                self.push_assistant_stream_delta(message_id, delta);
            },
            EventPayload::AssistantMessageCompleted { message_id, text } => {
                let streamed_to_scrollback = self.finish_assistant_stream(message_id, text);
                if let Some(message) = self.find_message_mut(message_id) {
                    message.body.set_text(text.clone());
                    message.is_streaming = false;
                    if !streamed_to_scrollback {
                        let completed = message.clone();
                        self.scrollback_queue
                            .push(ScrollbackEntry::Message(completed));
                    }
                    self.mark_dirty();
                } else {
                    // 未找到已有消息（可能错过了 Started 事件），直接创建
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
                // 不需要在消息记录中显示的工具仅更新状态栏
                if !tool_display::should_print_tool(tool_name) {
                    self.status = format!("Running {}", tool_name);
                    self.mark_dirty();
                    return;
                }
                let display = tool_display::started(tool_name);
                self.push_message(
                    MessageRole::Tool,
                    display.label,
                    display.body,
                    true,
                    Some(call_id.clone()),
                );
            },
            EventPayload::ToolCallArgumentsDelta { call_id, .. } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    let label = message.label.clone();
                    self.status = format!("Running {label}");
                    self.mark_dirty();
                }
            },
            EventPayload::ToolCallRequested {
                call_id,
                tool_name,
                arguments,
            } => {
                if !tool_display::should_print_tool(tool_name) {
                    self.status = format!("Running {}", tool_name);
                    self.mark_dirty();
                    return;
                }
                let display = tool_display::requested(tool_name, arguments);
                if let Some(message) = self.find_message_mut(call_id) {
                    message.label = display.label;
                    message.body.set_text(display.body);
                    self.mark_dirty();
                } else {
                    self.push_message(
                        MessageRole::Tool,
                        display.label,
                        display.body,
                        true,
                        Some(call_id.clone()),
                    );
                }
            },
            EventPayload::ToolOutputDelta { call_id, .. } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    let label = message.label.clone();
                    self.status = format!("Receiving {label}");
                    self.mark_dirty();
                }
            },
            EventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
            } => {
                let render_spec = ui_render_from_metadata(&result.metadata);
                // 隐藏工具的成功结果仅更新状态栏
                if !tool_display::should_print_tool(tool_name)
                    && !result.is_error
                    && render_spec.is_none()
                {
                    self.status = format!("{} completed", tool_name);
                    self.mark_dirty();
                    return;
                }
                let display = tool_display::completed(tool_name, result);

                if let Some(message) = self.find_message_mut(call_id) {
                    if let Some(spec) = render_spec {
                        let spec = tool_display::completed_render_spec(tool_name, spec, result);
                        message.body.set_render(spec, result.content.clone());
                    } else if !display.body.is_empty() && !message.body.contains_text(&display.body)
                    {
                        // 追加工具输出（去重）
                        if !message.body.is_empty() {
                            message.body.append_text("\n");
                        }
                        message.body.append_text(&display.body);
                    }
                    if result.is_error {
                        message.role = MessageRole::Error;
                        message.label = display.label;
                    }
                    message.is_streaming = false;
                    let completed = message.clone();
                    self.scrollback_queue
                        .push(ScrollbackEntry::Message(completed));
                    self.mark_dirty();
                } else if result.is_error {
                    // 工具错误但无已有消息记录，创建错误消息
                    self.push_message(
                        MessageRole::Error,
                        display.label,
                        display.body,
                        false,
                        Some(call_id.clone()),
                    );
                } else if let Some(spec) = render_spec {
                    let spec = tool_display::completed_render_spec(tool_name, spec, result);
                    self.push_render_message(
                        MessageRole::Tool,
                        display.label,
                        spec,
                        result.content.clone(),
                        Some(call_id.clone()),
                    );
                } else if !display.body.is_empty() {
                    self.push_message(
                        MessageRole::Tool,
                        display.label,
                        display.body,
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
                    message.body.set_text(format!(
                        "Compaction finished: {} -> {} tokens",
                        pre_tokens, post_tokens
                    ));
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

    /// 推入一条用户消息到消息记录。
    pub fn push_user(&mut self, text: &str) {
        self.scroll_transcript_to_bottom();
        self.push_message(MessageRole::User, "You".into(), text.into(), false, None);
    }

    /// 显示错误信息：更新状态栏并推入错误消息。
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

    /// 推入一条消息到消息记录列表。
    pub(crate) fn push_message(
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
        self.messages.push(msg);
        if !is_streaming {
            if let Some(last) = self.messages.last() {
                self.scrollback_queue
                    .push(ScrollbackEntry::Message(last.clone()));
            }
        }
        self.mark_dirty();
    }

    fn push_render_message(
        &mut self,
        role: MessageRole,
        label: String,
        spec: RenderSpec,
        fallback: String,
        key: Option<String>,
    ) {
        let mut body = MessageBody::text(String::new());
        body.set_render(spec, fallback);
        let msg = Message {
            role,
            label,
            body,
            is_streaming: false,
            key,
        };
        self.messages.push(msg.clone());
        self.scrollback_queue.push(ScrollbackEntry::Message(msg));
        self.mark_dirty();
    }

    fn push_assistant_stream_delta(&mut self, message_id: &str, delta: &str) {
        let Some(stream) = self.stream_scrollback.get_mut(message_id) else {
            return;
        };
        stream.seen_delta = true;
        stream.pending.push_str(delta);
        let chunks = stream.take_ready_chunks();
        self.push_stream_texts(MessageRole::Assistant, chunks);
    }

    fn finish_assistant_stream(&mut self, message_id: &str, completed_text: &str) -> bool {
        let Some(mut stream) = self.stream_scrollback.remove(message_id) else {
            return false;
        };
        if !stream.seen_delta {
            stream.pending.push_str(completed_text);
        }

        let mut chunks = stream.take_ready_chunks();
        if !stream.pending.trim().is_empty() {
            chunks.push(std::mem::take(&mut stream.pending));
        }
        self.push_stream_texts(MessageRole::Assistant, chunks);
        self.scrollback_queue.push(ScrollbackEntry::BlankLine);
        true
    }

    fn push_stream_texts(&mut self, role: MessageRole, chunks: Vec<String>) {
        for text in chunks {
            if text.is_empty() {
                self.scrollback_queue.push(ScrollbackEntry::BlankLine);
            } else {
                self.scrollback_queue.push(ScrollbackEntry::StreamText {
                    role: role.clone(),
                    text,
                });
            }
        }
    }

    /// 按 key 反向查找消息，返回可变引用。
    ///
    /// 从最新消息开始搜索，用于流式更新时定位已有消息。
    fn find_message_mut(&mut self, key: &str) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .rev()
            .find(|message| message.key.as_deref() == Some(key))
    }

    /// 根据输入内容同步斜杠命令面板状态。
    ///
    /// 输入以 `/` 开头时自动打开面板并提取过滤字符串，
    /// 输入不再以 `/` 开头时自动关闭面板。
    fn sync_slash_filter(&mut self) {
        let input = self.input_text();
        if input.starts_with('/') {
            let slash_filter = input
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            self.show_slash_palette = true;
            self.focus = Focus::SlashPalette;
            // 提取斜杠后的第一个词作为过滤条件
            self.slash_filter = slash_filter;
        } else if self.focus == Focus::SlashPalette {
            self.close_slash();
        }
    }
}

fn compact_inline(text: &str, max_chars: usize) -> String {
    tool_display::compact_inline(text, max_chars)
}

fn session_list_body(
    sessions: &[astrcode_protocol::events::SessionListItem],
    active_session_id: Option<&str>,
) -> String {
    if sessions.is_empty() {
        return "No sessions".into();
    }

    sessions
        .iter()
        .map(|session| {
            let marker = if active_session_id == Some(session.session_id.as_str()) {
                "*"
            } else {
                " "
            };
            let dir = if session.working_dir.is_empty() {
                "unknown".into()
            } else {
                compact_inline(&session.working_dir, 72)
            };
            format!("{marker} {} · {dir}", super::short_id(&session.session_id))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ui_render_from_metadata(metadata: &BTreeMap<String, serde_json::Value>) -> Option<RenderSpec> {
    metadata
        .get(UI_RENDER_METADATA_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{Event, EventPayload},
        render::{RenderKeyValue, RenderTone},
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

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::Tool);
        assert_eq!(state.messages[0].label, "Search");
        assert!(state.messages[0].body.plain_text().contains("matches: 1"));
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
        assert!(
            state.messages[0]
                .body
                .plain_text()
                .contains("command output")
        );
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
        assert_eq!(state.messages[0].label, "Glob");
        assert!(state.messages[0].body.plain_text().contains("glob failed"));
    }

    #[test]
    fn hidden_tool_with_ui_render_enters_transcript() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
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

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::Tool);
        assert!(state.messages[0].body.render_spec().is_some());
        assert_eq!(state.messages[0].body.plain_text(), "search complete");
    }

    #[test]
    fn assistant_deltas_enter_scrollback_incrementally() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
            EventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "first line\nsecond".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::AssistantMessageCompleted {
                message_id: "msg-1".into(),
                text: "first line\nsecond".into(),
            },
        );

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].body.plain_text(), "first line\nsecond");
        assert!(matches!(
            state.scrollback_queue.first(),
            Some(ScrollbackEntry::StreamHeader { label, .. }) if label == "Astrcode"
        ));
        assert!(state.scrollback_queue.iter().any(|entry| {
            matches!(entry, ScrollbackEntry::StreamText { text, .. } if text == "first line")
        }));
        assert!(state.scrollback_queue.iter().any(|entry| {
            matches!(entry, ScrollbackEntry::StreamText { text, .. } if text == "second")
        }));
        assert!(matches!(
            state.scrollback_queue.last(),
            Some(ScrollbackEntry::BlankLine)
        ));
        assert!(!state
            .scrollback_queue
            .iter()
            .any(|entry| matches!(entry, ScrollbackEntry::Message(message) if message.role == MessageRole::Assistant)));
    }

    #[test]
    fn markdown_like_assistant_stream_is_not_reflowed_as_completed_message() {
        let mut state = TuiState::new();

        apply_payload(
            &mut state,
            EventPayload::AssistantMessageStarted {
                message_id: "msg-1".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::AssistantTextDelta {
                message_id: "msg-1".into(),
                delta: "# Title\n- item".into(),
            },
        );
        apply_payload(
            &mut state,
            EventPayload::AssistantMessageCompleted {
                message_id: "msg-1".into(),
                text: "# Title\n- item".into(),
            },
        );

        assert!(state.scrollback_queue.iter().any(|entry| {
            matches!(entry, ScrollbackEntry::StreamText { text, .. } if text == "# Title")
        }));
        assert!(state.scrollback_queue.iter().any(|entry| {
            matches!(entry, ScrollbackEntry::StreamText { text, .. } if text == "- item")
        }));
        assert!(!state
            .scrollback_queue
            .iter()
            .any(|entry| matches!(entry, ScrollbackEntry::Message(message) if message.role == MessageRole::Assistant)));
    }

    #[test]
    fn input_history_recalls_prompts_and_commands() {
        let mut state = TuiState::new();

        state.remember_input("first prompt");
        state.remember_input("/sessions");

        state.history_previous();
        assert_eq!(state.input_text(), "/sessions");
        assert_eq!(state.focus, Focus::SlashPalette);

        state.history_previous();
        assert_eq!(state.input_text(), "first prompt");
        assert_eq!(state.focus, Focus::Input);

        state.history_next();
        assert_eq!(state.input_text(), "/sessions");

        state.history_next();
        assert!(state.input_text().is_empty());
    }
}
