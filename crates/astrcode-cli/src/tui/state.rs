//! TUI 状态管理 —— 消息记录和输入编辑器的状态模型。
//!
//! 维护消息列表、输入框内容与光标、会话信息、斜杠命令面板状态等，
//! 并提供将服务器事件（ClientNotification）应用到 UI 状态的方法。

use std::collections::BTreeMap;

use astrcode_core::{
    event::{Event, EventPayload},
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::ToolResult,
};
use astrcode_protocol::events::ClientNotification;

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

    fn push_newline_if_needed(&mut self) {
        if !self.plain.ends_with('\n') {
            self.plain.push('\n');
        }
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
    /// 输入框当前文本内容
    pub input: String,
    /// 输入框光标位置（以 char 索引计）
    pub input_cursor: usize,
    /// 历史输入记录（用于上下翻页浏览）
    pub input_history: Vec<String>,
    /// 当前历史浏览位置索引
    pub input_history_idx: Option<usize>,
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
}

impl TuiState {
    /// 创建初始 TUI 状态，所有字段设为默认值。
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            transcript_scroll: 0,
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

    /// 标记状态为脏，触发下一帧重绘。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 在光标位置插入一个字符。
    pub fn insert_char(&mut self, ch: char) {
        let byte_idx = self.cursor_byte_index();
        self.input.insert(byte_idx, ch);
        self.input_cursor += 1;
        self.sync_slash_filter();
        self.mark_dirty();
    }

    /// 在光标位置插入换行符。
    pub fn insert_newline(&mut self) {
        let byte_idx = self.cursor_byte_index();
        self.input.insert(byte_idx, '\n');
        self.input_cursor += 1;
        self.sync_slash_filter();
        self.mark_dirty();
    }

    /// 删除光标前一个字符（退格键）。
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

    /// 删除光标位置的字符（Delete 键）。
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

    /// 光标左移一个字符。
    pub fn move_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
        self.mark_dirty();
    }

    /// 光标右移一个字符，不超过文本末尾。
    pub fn move_right(&mut self) {
        self.input_cursor = (self.input_cursor + 1).min(self.input.chars().count());
        self.mark_dirty();
    }

    /// 光标移动到输入开头。
    pub fn move_home(&mut self) {
        self.input_cursor = 0;
        self.mark_dirty();
    }

    /// 光标移动到输入末尾。
    pub fn move_end(&mut self) {
        self.input_cursor = self.input.chars().count();
        self.mark_dirty();
    }

    /// 替换输入框内容并将光标移到末尾。
    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.chars().count();
        self.sync_slash_filter();
        self.mark_dirty();
    }

    /// 取出输入框内容，清空输入并重置相关状态。
    ///
    /// 返回被取出的文本内容。
    pub fn take_input(&mut self) -> String {
        self.input_history_idx = None;
        self.close_slash();
        self.input_cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// 将输入记录到历史列表（去重，不记录空输入）。
    pub fn remember_input(&mut self, input: &str) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }
        // 避免连续重复记录
        if self.input_history.last().map(|v| v.as_str()) != Some(trimmed) {
            self.input_history.push(trimmed.to_string());
        }
    }

    /// 浏览上一条历史输入。
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

    /// 浏览下一条历史输入，到达末尾后清空输入框。
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

    /// 向上滚动消息记录区。
    pub fn scroll_transcript_up(&mut self, lines: usize) {
        self.transcript_scroll = self.transcript_scroll.saturating_add(lines);
        self.mark_dirty();
    }

    /// 向下滚动消息记录区。
    pub fn scroll_transcript_down(&mut self, lines: usize) {
        self.transcript_scroll = self.transcript_scroll.saturating_sub(lines);
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
            // 用户消息在按下 Enter 时已乐观推入，此处无需处理
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
                    message.body.append_text(delta);
                    self.mark_dirty();
                }
            },
            EventPayload::AssistantMessageCompleted { message_id, text } => {
                if let Some(message) = self.find_message_mut(message_id) {
                    message.body.set_text(text.clone());
                    message.is_streaming = false;
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
                if !should_print_tool(tool_name) {
                    self.status = format!("Running {}", tool_name);
                    self.mark_dirty();
                    return;
                }
                self.push_message(
                    MessageRole::Tool,
                    tool_message_label(tool_name, None),
                    tool_name.clone(),
                    true,
                    Some(call_id.clone()),
                );
            },
            EventPayload::ToolCallArgumentsDelta { call_id, delta } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    message.body.push_newline_if_needed();
                    message.body.append_text(delta);
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
                let body = tool_request_body(tool_name, arguments);
                if let Some(message) = self.find_message_mut(call_id) {
                    message.label = tool_message_label(tool_name, Some(arguments));
                    message.body.set_text(body);
                    self.mark_dirty();
                } else {
                    self.push_message(
                        MessageRole::Tool,
                        tool_message_label(tool_name, Some(arguments)),
                        body,
                        true,
                        Some(call_id.clone()),
                    );
                }
            },
            EventPayload::ToolOutputDelta { call_id, delta, .. } => {
                if let Some(message) = self.find_message_mut(call_id) {
                    if !message.body.is_empty() {
                        message.body.append_text("\n");
                    }
                    message.body.append_text(delta);
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
                if !should_print_tool(tool_name) && !result.is_error && render_spec.is_none() {
                    self.status = format!("{} completed", tool_name);
                    self.mark_dirty();
                    return;
                }

                if let Some(message) = self.find_message_mut(call_id) {
                    if let Some(spec) = render_spec {
                        let spec = completed_tool_render_spec(tool_name, spec, result);
                        message.body.set_render(spec, result.content.clone());
                    } else if !result.content.is_empty()
                        && !message.body.contains_text(&result.content)
                    {
                        // 追加工具输出（去重）
                        if !message.body.is_empty() {
                            message.body.append_text("\n");
                        }
                        message.body.append_text(&result.content);
                    }
                    if result.is_error {
                        message.role = MessageRole::Error;
                        message.label = "Tool Error".into();
                    }
                    message.is_streaming = false;
                    self.mark_dirty();
                } else if result.is_error {
                    // 工具错误但无已有消息记录，创建错误消息
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
                } else if let Some(spec) = render_spec {
                    let spec = completed_tool_render_spec(tool_name, spec, result);
                    self.push_message(
                        if result.is_error {
                            MessageRole::Error
                        } else {
                            MessageRole::Tool
                        },
                        if result.is_error {
                            "Tool Error".into()
                        } else {
                            tool_message_label(tool_name, None)
                        },
                        result.content.clone(),
                        false,
                        Some(call_id.clone()),
                    );
                    if let Some(message) = self.find_message_mut(call_id) {
                        message.body.set_render(spec, result.content.clone());
                    }
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
        self.messages.push(Message {
            role,
            label,
            body: MessageBody::text(content),
            is_streaming,
            key,
        });
        self.mark_dirty();
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
        if self.input.starts_with('/') {
            self.show_slash_palette = true;
            self.focus = Focus::SlashPalette;
            // 提取斜杠后的第一个词作为过滤条件
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

    /// 将 char 索引的光标位置转换为 byte 索引，用于字符串插入/删除。
    fn cursor_byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.input_cursor)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.input.len())
    }
}

/// 判断工具是否应在消息记录中显示。
///
/// shell、agent、editFile、applyPatch 等工具的输出对用户有直接价值，需要显示；
/// 其他工具（如搜索类）仅更新状态栏，避免刷屏。
fn should_print_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell" | "agent" | "editFile" | "apply_patch" | "applyPatch"
    )
}

fn tool_message_label(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    if tool_name == "agent" {
        if let Some(description) = arguments
            .and_then(|value| value["description"].as_str())
            .filter(|description| !description.trim().is_empty())
        {
            return format!("Task({description})");
        }
        return "Task".into();
    }

    format!("Tool · {tool_name}")
}

fn tool_request_body(tool_name: &str, arguments: &serde_json::Value) -> String {
    if tool_name == "agent" {
        let mut lines = Vec::new();
        if let Some(subagent_type) = arguments["subagent_type"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("subagent: {subagent_type}"));
        }
        if let Some(prompt) = arguments["prompt"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("prompt: {}", compact_inline(prompt, 180)));
        }
        return if lines.is_empty() {
            "agent".into()
        } else {
            lines.join("\n")
        };
    }

    let args = serde_json::to_string(arguments).unwrap_or_default();
    if args.is_empty() || args == "{}" {
        tool_name.into()
    } else {
        format!("{tool_name}\n{args}")
    }
}

fn compact_inline(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

fn completed_tool_render_spec(
    tool_name: &str,
    spec: RenderSpec,
    result: &ToolResult,
) -> RenderSpec {
    if tool_name == "agent" {
        agent_done_render_spec(spec, result)
    } else {
        spec
    }
}

fn agent_done_render_spec(spec: RenderSpec, result: &ToolResult) -> RenderSpec {
    let mut children = match spec {
        RenderSpec::Box { children, .. } => children,
        spec => vec![spec],
    };

    if let Some(child_session_id) = result
        .metadata
        .get("child_session_id")
        .and_then(|value| value.as_str())
    {
        children.push(RenderSpec::KeyValue {
            entries: vec![RenderKeyValue {
                key: "session".into(),
                value: child_session_id.into(),
                tone: RenderTone::Muted,
            }],
            tone: RenderTone::Default,
        });
    }

    if !result.content.trim().is_empty() {
        children.push(RenderSpec::Markdown {
            text: result.content.clone(),
            tone: if result.is_error {
                RenderTone::Error
            } else {
                RenderTone::Default
            },
        });
    }

    RenderSpec::Box {
        title: Some(if result.is_error {
            "Failed".into()
        } else {
            "Done".into()
        }),
        tone: if result.is_error {
            RenderTone::Error
        } else {
            RenderTone::Success
        },
        children,
    }
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
        assert_eq!(state.messages[0].body.plain_text(), "glob failed");
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
    fn input_history_recalls_prompts_and_commands() {
        let mut state = TuiState::new();

        state.remember_input("first prompt");
        state.remember_input("/sessions");

        state.history_previous();
        assert_eq!(state.input, "/sessions");
        assert_eq!(state.focus, Focus::SlashPalette);

        state.history_previous();
        assert_eq!(state.input, "first prompt");
        assert_eq!(state.focus, Focus::Input);

        state.history_next();
        assert_eq!(state.input, "/sessions");

        state.history_next();
        assert!(state.input.is_empty());
    }
}
