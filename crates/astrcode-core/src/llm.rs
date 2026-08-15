//! LLM 提供者抽象与消息类型。
//!
//! 本模块定义了与 LLM 交互所需的核心类型：
//! - [`LlmMessage`] / [`LlmContent`]：对话消息和内容类型
//! - [`LlmEvent`]：LLM 流式输出事件
//! - [`LlmProvider`] trait：所有 LLM 后端的统一接口
//! - [`LlmClientConfig`]：LLM 客户端配置
//! - [`LlmError`]：LLM 操作错误类型
//!
//! 本模块不含具体 provider 实现与 HTTP/重试逻辑（位于 `astrcode-ai`），也不含
//! 具体工具的展示特判（属于 server 投影层）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{message_attachment::MessageAttachment, tool::ToolDefinition};

pub mod thinking;
pub mod token_estimate;

use thinking::{ThinkingCapability, ThinkingConfig};

/// LLM 对话消息中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmRole {
    /// 系统指令消息。
    System,
    /// 用户消息。
    User,
    /// 助手回复消息。
    Assistant,
    /// 工具结果消息。
    Tool,
}

impl LlmRole {
    /// 返回角色的字符串表示，用于协议序列化。
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
    }
}

/// LLM 消息的内容——可以是文本、图片、工具调用或工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmContent {
    /// 纯文本内容。
    Text { text: String },
    /// Base64 编码的图片。
    Image {
        /// Base64 编码的图片数据。
        base64: String,
        /// 图片的 MIME 类型（如 "image/png"）。
        media_type: String,
        /// 原始文件名；旧持久化记录可能缺失。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// 助手请求的工具调用内容。
    ToolCall {
        /// 工具调用的唯一标识。
        call_id: String,
        /// 要调用的工具名称。
        name: String,
        /// 工具调用参数（JSON 值）。
        arguments: serde_json::Value,
        /// Provider 返回但无法解析的原始参数。旧记录和合法 JSON 参数为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
    },
    /// 工具执行结果内容。
    ToolResult {
        /// 对应的工具调用 ID。
        tool_call_id: String,
        /// 工具输出的内容文本。
        content: String,
        /// 是否为错误结果。
        is_error: bool,
    },
}

impl LlmContent {
    /// 返回纯文本内容；非文本块返回 `None`。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// 按给定分隔符拼接内容集合中的纯文本块，忽略其他内容类型。
    pub fn join_text<'a>(contents: impl IntoIterator<Item = &'a Self>, separator: &str) -> String {
        let mut output = String::new();
        for (index, text) in contents.into_iter().filter_map(Self::as_text).enumerate() {
            if index > 0 {
                output.push_str(separator);
            }
            output.push_str(text);
        }
        output
    }

    /// 将内容转换为人类可读的纯文本展示。
    ///
    /// 这是有损转换——不可能完全还原原始渲染效果。
    /// - `Text` / `ToolResult`：原样输出。
    /// - `Image`：返回占位符 `[image]`。
    /// - `ToolCall`：只输出工具名；具体工具的展示特判属于投影层（如 server 对 `upsertSessionPlan`
    ///   提取 plan 正文），不在契约层硬编码。
    pub fn to_display_text(&self) -> String {
        match self {
            LlmContent::Text { text } => text.clone(),
            LlmContent::Image { .. } => "[image]".into(),
            LlmContent::ToolCall { name, .. } => format!("tool call: {name}"),
            LlmContent::ToolResult { content, .. } => content.clone(),
        }
    }
}

/// LLM 对话中的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmMessage {
    /// 消息角色。
    pub role: LlmRole,
    /// 消息内容列表（支持混合文本、图片、工具调用等）。
    pub content: Vec<LlmContent>,
    /// 可选的工具消息名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 推理内容（仅 assistant 消息）。部分 provider（如 DeepSeek）要求将此字段回传。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// Provider 不可见、但会随 transcript rewrite 与 fork 持久化的消息来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptMessageOrigin {
    TurnAborted,
    ToolCallFailed,
    ToolCallCancelled,
}

/// 一条 durable provider transcript 消息及其非 provider 元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptMessage {
    pub message: LlmMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<TranscriptMessageOrigin>,
}

impl TranscriptMessage {
    pub fn plain(message: LlmMessage) -> Self {
        Self {
            message,
            origin: None,
        }
    }
}

/// 运行时共享的 transcript 消息：与 [`TranscriptMessage`] 同构，但消息体经 `Arc`
/// 跨读模型与请求组装共享。
///
/// 共享消息构造后即不可变；任何改写必须先 `Arc::make_mut` copy-on-write，
/// 否则改动会跨快照泄漏。持久化/wire 格式不使用本类型。
#[derive(Debug, Clone)]
pub struct SharedTranscriptMessage {
    pub message: Arc<LlmMessage>,
    pub origin: Option<TranscriptMessageOrigin>,
}

impl LlmMessage {
    /// 创建一条用户文本消息。
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }

    /// 由文本与附件组装用户消息（图片走 [`LlmContent::Image`]，
    /// 非图片附件用 XML 分隔符包装为文本块）。
    pub fn user_with_attachments(text: &str, attachments: &[MessageAttachment]) -> Self {
        let mut content = Vec::new();
        for att in attachments {
            if att.is_image() {
                content.push(LlmContent::Image {
                    base64: att.content.clone(),
                    media_type: att.media_type.clone(),
                    filename: Some(att.filename.clone()),
                });
            } else {
                content.push(LlmContent::Text {
                    text: format!(
                        "<attachment filename=\"{}\" media_type=\"{}\">\n{}\n</attachment>",
                        xml_escape_attr(&att.filename),
                        xml_escape_attr(&att.media_type),
                        att.content
                    ),
                });
            }
        }
        if !text.is_empty() {
            content.push(LlmContent::Text {
                text: text.to_string(),
            });
        }
        if content.is_empty() {
            content.push(LlmContent::Text {
                text: String::new(),
            });
        }
        Self {
            role: LlmRole::User,
            content,
            name: None,
            reasoning_content: None,
        }
    }

    /// 创建一条助手文本消息。
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::Assistant,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }

    /// 创建一条系统指令消息。
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::System,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }

    /// 创建一条工具结果消息。
    ///
    /// - `name`：工具名称
    /// - `tool_call_id`：对应的工具调用 ID
    /// - `content`：工具输出的内容
    /// - `is_error`：是否为错误结果
    pub fn tool(
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: content.into(),
                is_error,
            }],
            name: Some(name.into()),
            reasoning_content: None,
        }
    }

    /// 将各 content 块经 [`LlmContent::to_display_text`] 转换后用 `separator` 拼接。
    pub fn joined_display_text(&self, separator: &str) -> String {
        self.content
            .iter()
            .map(LlmContent::to_display_text)
            .collect::<Vec<_>>()
            .join(separator)
    }

    /// 按给定分隔符拼接消息中的纯文本块，忽略图片和工具块。
    pub fn joined_text(&self, separator: &str) -> String {
        LlmContent::join_text(&self.content, separator)
    }

    /// 判断该消息在去掉展示元数据后是否仍应发送给 provider。
    pub fn has_provider_visible_content(&self) -> bool {
        if self.content.iter().any(|content| match content {
            LlmContent::Text { text } => !text.trim().is_empty(),
            LlmContent::Image { .. }
            | LlmContent::ToolCall { .. }
            | LlmContent::ToolResult { .. } => true,
        }) {
            return true;
        }
        self.reasoning_content
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty())
    }
}

fn xml_escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// 从用户 LLM 消息中提取附件（与 [`LlmMessage::user_with_attachments`] 对称）。
pub fn attachments_from_user_message(message: &LlmMessage) -> Vec<MessageAttachment> {
    message
        .content
        .iter()
        .enumerate()
        .filter_map(|(index, content)| match content {
            LlmContent::Image {
                base64,
                media_type,
                filename,
            } => Some(MessageAttachment {
                filename: filename
                    .clone()
                    .unwrap_or_else(|| format!("image-{}.png", index + 1)),
                content: base64.clone(),
                media_type: media_type.clone(),
            }),
            _ => None,
        })
        .collect()
}

const TURN_ABORTED_GUIDANCE: &str = concat!(
    "The user interrupted the previous turn on purpose. ",
    "Any running tools/commands may still be running in the background. ",
    "If any tools/commands were aborted, they may have partially executed."
);

pub fn turn_aborted_context_message() -> LlmMessage {
    LlmMessage::user(format!(
        "<turn_aborted>\n{}\n</turn_aborted>",
        TURN_ABORTED_GUIDANCE
    ))
}

/// 返回 provider 可见且满足 tool-call 协议的消息序列。
///
/// OpenAI Chat Completions 要求 assistant 的 `tool_calls` 后面紧跟对应的
/// tool result。这里是所有 provider request 的最后一道边界，负责过滤空消息、
/// 合并旧日志中的拆分 assistant/tool-call 消息，并裁掉尚未结算的半轮工具调用。
pub fn provider_visible_messages(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    provider_visible_entries(messages)
}

/// [`provider_visible_messages`] 的共享版本：未受归一化影响的消息零拷贝复用，
/// 仅被合并的 assistant 消息经 `Arc::make_mut` copy-on-write，不写穿共享快照。
pub fn provider_visible_shared_messages(messages: Vec<Arc<LlmMessage>>) -> Vec<Arc<LlmMessage>> {
    provider_visible_entries(messages)
}

/// 返回指纹与 compact 共用的 provider transcript：provider 可见消息中剔除 System 角色。
///
/// system prompt 单独参与指纹与请求组装（见
/// `crate::event::transcript_prefix_fingerprint`），因此 transcript 只保留对话消息。
pub fn provider_transcript(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    provider_transcript_messages(messages.into_iter().map(TranscriptMessage::plain).collect())
        .into_iter()
        .map(|entry| entry.message)
        .collect()
}

/// 归一化 durable transcript，同时保持每条保留消息的来源元数据。
pub fn provider_transcript_messages(messages: Vec<TranscriptMessage>) -> Vec<TranscriptMessage> {
    let mut messages = provider_visible_entries(messages);
    messages.retain(|entry| entry.message.role != LlmRole::System);
    messages
}

/// [`provider_transcript_messages`] 的共享版本，语义与 owned 路径一致。
pub fn provider_transcript_shared_messages(
    messages: Vec<SharedTranscriptMessage>,
) -> Vec<SharedTranscriptMessage> {
    let mut messages = provider_visible_entries(messages);
    messages.retain(|entry| entry.message.role != LlmRole::System);
    messages
}

/// `provider_visible_*` 归一化的条目抽象，让 owned 与 `Arc` 共享条目共用同一套
/// 过滤/合并/截断逻辑，保证两条路径输出一致。
trait ProviderVisibleEntry {
    fn message(&self) -> &LlmMessage;
    /// 将 `other`(assistant + tool calls)并入 self;`other` 随即被丢弃。
    fn absorb_assistant(&mut self, other: Self);
}

impl ProviderVisibleEntry for LlmMessage {
    fn message(&self) -> &LlmMessage {
        self
    }

    fn absorb_assistant(&mut self, mut other: Self) {
        self.content.append(&mut other.content);
        if self.reasoning_content.is_none() {
            self.reasoning_content = other.reasoning_content;
        }
    }
}

impl ProviderVisibleEntry for TranscriptMessage {
    fn message(&self) -> &LlmMessage {
        &self.message
    }

    fn absorb_assistant(&mut self, mut other: Self) {
        self.message.content.append(&mut other.message.content);
        if self.message.reasoning_content.is_none() {
            self.message.reasoning_content = other.message.reasoning_content;
        }
        self.origin = self.origin.or(other.origin);
    }
}

impl ProviderVisibleEntry for Arc<LlmMessage> {
    fn message(&self) -> &LlmMessage {
        self
    }

    fn absorb_assistant(&mut self, other: Self) {
        Arc::make_mut(self).append(other.message());
    }
}

impl ProviderVisibleEntry for SharedTranscriptMessage {
    fn message(&self) -> &LlmMessage {
        &self.message
    }

    fn absorb_assistant(&mut self, other: Self) {
        Arc::make_mut(&mut self.message).append(&other.message);
        self.origin = self.origin.or(other.origin);
    }
}

impl LlmMessage {
    /// 并入一条 assistant + tool calls 消息（`provider_visible_*` 的合并分支）。
    fn append(&mut self, other: &Self) {
        self.content.extend(other.content.iter().cloned());
        if self.reasoning_content.is_none() {
            self.reasoning_content = other.reasoning_content.clone();
        }
    }
}

fn provider_visible_entries<E: ProviderVisibleEntry>(messages: Vec<E>) -> Vec<E> {
    let mut messages: Vec<E> = messages
        .into_iter()
        .filter(|entry| entry.message().has_provider_visible_content())
        .collect();
    normalize_tool_call_entries(&mut messages);
    truncate_incomplete_tool_entries(&mut messages);
    messages
}

fn normalize_tool_call_entries<E: ProviderVisibleEntry>(messages: &mut Vec<E>) {
    let mut merged: Vec<E> = Vec::with_capacity(messages.len());
    for entry in messages.drain(..) {
        let has_tool_calls = entry.message().role == LlmRole::Assistant
            && entry
                .message()
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. }));
        if has_tool_calls
            && let Some(last) = merged.last_mut()
            && last.message().role == LlmRole::Assistant
        {
            last.absorb_assistant(entry);
            continue;
        }
        merged.push(entry);
    }
    *messages = merged;
}

fn truncate_incomplete_tool_entries<E: ProviderVisibleEntry>(messages: &mut Vec<E>) {
    use std::collections::HashSet;

    let mut pending: Option<(usize, HashSet<String>, HashSet<String>)> = None;

    for index in 0..messages.len() {
        let message = messages[index].message();
        if message.role == LlmRole::Tool {
            let tool_result_ids: Vec<String> = message
                .content
                .iter()
                .filter_map(|content| match content {
                    LlmContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                    _ => None,
                })
                .collect();
            if tool_result_ids.is_empty() {
                messages.truncate(index);
                return;
            }
            let Some((_, call_ids, answered)) = pending.as_mut() else {
                messages.truncate(index);
                return;
            };
            for tool_call_id in tool_result_ids {
                if !call_ids.contains(&tool_call_id) || answered.contains(&tool_call_id) {
                    messages.truncate(index);
                    return;
                }
                answered.insert(tool_call_id);
            }
            if call_ids.iter().all(|id| answered.contains(id)) {
                pending = None;
            }
            continue;
        }

        if let Some((start, _, _)) = pending {
            messages.truncate(start);
            return;
        }

        if message.role == LlmRole::Assistant {
            let call_ids: HashSet<String> = message
                .content
                .iter()
                .filter_map(|content| match content {
                    LlmContent::ToolCall { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect();
            if !call_ids.is_empty() {
                pending = Some((index, call_ids, HashSet::new()));
            }
        }
    }

    if let Some((start, _, _)) = pending {
        messages.truncate(start);
    }
}

/// 单次 LLM 调用的 token 使用统计。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// How provider input and cache counters relate to each other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_accounting: Option<LlmInputTokenAccounting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LlmTokenUsageSource>,
}

/// Provider usage counter semantics, normalized at the provider wire boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmInputTokenAccounting {
    /// `input_tokens` contains cached input; cache counters are descriptive subsets.
    Inclusive,
    /// Regular, cache-read, and cache-creation inputs are independent components.
    Components,
}

/// token usage 的来源，用于区分 provider 原生统计与 fallback 估算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmTokenUsageSource {
    ProviderUsage,
    ProviderCount,
    ProviderCountFallback,
    LocalEstimateFallback,
}

/// provider 预请求 input token 统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInputTokenCount {
    pub input_tokens: u64,
    pub source: LlmTokenUsageSource,
}

impl ProviderInputTokenCount {
    pub fn provider_count(input_tokens: u64) -> Self {
        Self {
            input_tokens,
            source: LlmTokenUsageSource::ProviderCount,
        }
    }
}

impl LlmTokenUsage {
    /// Returns billable non-cache-read input plus generated output.
    pub fn non_cached_tokens(&self) -> Option<u64> {
        let cached = self.cached_input_tokens.unwrap_or_default();
        let component_accounting = matches!(
            self.input_accounting,
            Some(LlmInputTokenAccounting::Components)
        ) || (self.input_accounting.is_none()
            && self.cache_creation_input_tokens.is_some());
        // Persisted usage predating `input_accounting` only populated cache-creation tokens for
        // component-style providers.
        if component_accounting {
            let input = self
                .input_tokens
                .unwrap_or_default()
                .saturating_add(self.cache_creation_input_tokens.unwrap_or_default());
            return (self.input_tokens.is_some()
                || self.cache_creation_input_tokens.is_some()
                || self.output_tokens.is_some())
            .then(|| input.saturating_add(self.output_tokens.unwrap_or_default()));
        }

        self.total_tokens
            .map(|total| total.saturating_sub(cached))
            .or_else(|| {
                let input = self.input_tokens.map(|input| input.saturating_sub(cached));
                (input.is_some() || self.output_tokens.is_some()).then(|| {
                    input
                        .unwrap_or_default()
                        .saturating_add(self.output_tokens.unwrap_or_default())
                })
            })
    }

    /// 返回本次响应结束后占用的完整上下文 token。
    ///
    /// Provider 原生 `total_tokens` 优先；Anthropic 等未提供 total 的协议只有在
    /// input/output 都存在时才可形成可靠锚点。缓存字段在不同 provider 中可能是
    /// input 的子集或独立分量，缺少 total 时宁可保守相加。
    pub fn context_tokens_after_response(&self) -> Option<u64> {
        if let Some(total_tokens) = self.total_tokens.filter(|tokens| *tokens > 0) {
            return Some(total_tokens);
        }

        let input = self.input_tokens?;
        let output = self.output_tokens?;
        Some(match self.input_accounting {
            Some(LlmInputTokenAccounting::Inclusive) => input.saturating_add(output),
            Some(LlmInputTokenAccounting::Components) | None => input
                .saturating_add(self.cached_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(output),
        })
    }
}

/// 单次 LLM 生成请求。
///
/// `max_output_tokens` 是请求级上限；缺失时 provider 使用模型配置上限。该字段必须
/// 在最终消息和工具确定后计算，避免 input 与固定 output 上限共同挤爆上下文窗口。
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<usize>,
}

impl LlmRequest {
    pub fn new(messages: Vec<LlmMessage>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            messages,
            tools,
            max_output_tokens: None,
        }
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}

/// LLM 流式输出过程中的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmEvent {
    /// 远端返回瞬态 HTTP 或传输错误，当前请求将在退避后重试。
    Retrying {
        /// HTTP 状态码；连接或响应流中断时为空。
        status: Option<u16>,
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
    },
    /// 重试后已重新取得成功的 HTTP 响应。
    RetryRecovered,
    /// 文本增量（部分响应）。
    ContentDelta { delta: String },
    /// 推理模型思维链增量。
    ThinkingDelta { delta: String },
    /// 工具调用已开始。
    ToolCallStart {
        /// 工具调用 ID。
        call_id: String,
        /// 工具名称。
        name: String,
        /// 初始参数片段。
        arguments: String,
    },
    /// 工具调用参数增量。
    ToolCallDelta {
        /// 工具调用 ID。
        call_id: String,
        /// 本次增量参数片段。
        delta: String,
    },
    /// 单个工具调用的参数已全部接收完毕。
    ///
    /// 此事件允许下游在 LLM 仍在流式输出其他工具调用时就提前准备和执行
    /// 已完成的工具,从而缩短多工具调用场景的端到端延迟。
    ToolCallCompleted {
        /// 工具调用 ID。
        call_id: String,
    },
    /// 单次 LLM 调用的 token 使用统计。
    Usage { usage: LlmTokenUsage },
    /// 流式输出已完成。
    Done { finish_reason: String },
    /// 流式输出过程中发生错误。
    Error { message: String },
}

/// LLM 提供者操作产生的错误。
///
/// 跨 provider 的统一错误分类。HTTP 状态码与响应体在边界(`astrcode-ai` 的
/// `classify_error`)归一化为这些变体;[`LlmError::is_retryable`] 是连接期
/// 是否重试的唯一事实来源。同时实现 serde,以便将来作为结构化错误跨边界传输。
#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmError {
    /// API key 无效或缺失(401/403)。
    #[error("invalid api key ({status}): {message}")]
    InvalidApiKey { status: u16, message: String },
    /// 模型不存在(404)。
    #[error("model not found ({status}): {message}")]
    ModelNotFound { status: u16, message: String },
    /// 请求参数无效(400/422)。
    #[error("invalid parameter ({status}): {message}")]
    InvalidParameter { status: u16, message: String },
    /// 配额/计费耗尽(402,或 429 的配额类响应)。
    #[error("quota exceeded ({status}): {message}")]
    QuotaExceeded { status: u16, message: String },
    /// 提示词超出模型上下文长度限制。
    #[error("context window exceeded: {message}")]
    ContextWindowExceeded { message: String },
    /// 被限流(429)。`retry_after_ms` 来自 `Retry-After` 响应头(若提供)。
    #[error("rate limited ({status}): {message}")]
    RateLimited {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
        message: String,
    },
    /// 其他客户端错误(4xx)。
    #[error("client error ({status}): {message}")]
    ClientError { status: u16, message: String },
    /// 服务端错误(5xx,含 408 超时)。
    #[error("server error ({status}): {message}")]
    ServerError { status: u16, message: String },
    /// 网络传输错误(连接、TLS、DNS 等)。
    #[error("transport error: {message}")]
    Transport { message: String },
    /// 流式响应意外断开。
    #[error("stream disconnected: {message}")]
    StreamDisconnected { message: String },
    /// 流式响应解析错误。
    #[error("stream parse error: {message}")]
    StreamParse { message: String },
    /// 响应被内容安全策略过滤。
    #[error("content filtered: {message}")]
    ContentFilter { message: String },
    /// 输出 token 超出限制。
    #[error("output token limit: {message}")]
    TokenLimit { message: String },
    /// 模型返回空响应。
    #[error("empty response")]
    EmptyResponse,
    /// 请求被中断(用户取消)。
    #[error("request interrupted")]
    Interrupted,
    /// 当前 provider 不支持该操作(能力缺口,非 HTTP 错误分类)。
    #[error("unsupported LLM operation: {message}")]
    Unsupported { message: String },
}

impl LlmError {
    /// 便捷构造传输错误。
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }

    /// 便捷构造流解析错误。
    pub fn stream_parse(message: impl Into<String>) -> Self {
        Self::StreamParse {
            message: message.into(),
        }
    }

    /// 连接期重试是否值得;流内失败按设计不重试(无中途恢复)。
    ///
    /// 与 `astrcode-ai::retry::RetryPolicy::should_retry` 的状态码集合一致
    /// (429→`RateLimited`、5xx/408→`ServerError`);重试发生在 HTTP 层、在错误分类之前,
    /// 因此两处状态码清单需同步维护。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport { .. } => true,
            Self::ServerError { status, .. } => matches!(status, 408 | 500 | 502 | 503 | 504),
            _ => false,
        }
    }
}

/// OpenAI prompt cache retention 声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheRetention {
    /// 使用服务端默认的短期内存缓存。
    InMemory,
    /// 请求保留更长的 24 小时缓存。
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// 推理强度级别（跨模型选项的标准化抽象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Low,
    Medium,
    High,
}

/// OpenAI 兼容 API 的 provider 特有选项（prompt cache 等）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAiProviderExtras {
    /// 当前 provider 是否支持 OpenAI `prompt_cache_key`。
    pub supports_prompt_cache_key: bool,
    /// 当前 provider 是否支持流式 usage 统计。
    pub supports_stream_usage: bool,
    /// 可选的 OpenAI prompt cache retention。
    pub prompt_cache_retention: Option<PromptCacheRetention>,
}

/// Provider 特有配置；通用字段留在 [`LlmClientConfig`]。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderExtras {
    #[default]
    None,
    OpenAi(OpenAiProviderExtras),
}

/// LLM 客户端配置（跨 provider 通用字段 + [`ProviderExtras`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmClientConfig {
    /// API 端点的基础 URL。
    pub base_url: String,
    /// API 密钥。
    pub api_key: String,
    /// API key 的鉴权方式。
    pub auth_scheme: crate::config::ProviderAuthScheme,
    /// 连接超时时间（秒）。
    pub connect_timeout_secs: u64,
    /// 读取超时时间（秒）。
    pub read_timeout_secs: u64,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 指数退避的基础延迟（毫秒）。
    pub retry_base_delay_ms: u64,
    /// 当前 profile 是否允许把工具的 strict 声明发送给 provider。
    pub supports_strict_tool_use: bool,
    /// Provider 特有选项。
    pub extras: ProviderExtras,
    /// 额外的 HTTP 请求头。
    pub extra_headers: std::collections::HashMap<String, String>,
    /// 标准化 thinking 配置。
    #[serde(default)]
    pub thinking: ThinkingConfig,
    /// 决定 thinking 配置如何映射到当前模型的 wire 请求。
    #[serde(default)]
    pub thinking_capability: Option<ThinkingCapability>,
    /// 是否显式配置了 thinking；false 时 provider 必须使用模型默认值并省略参数。
    #[serde(default)]
    pub thinking_configured: bool,
}

impl LlmClientConfig {
    /// 从解析后的 [`LlmSettings`](crate::config::LlmSettings) 构造客户端配置。
    ///
    /// 两个结构都在本 crate，由本函数集中完成边界映射；调用方不再逐字段拷贝。
    /// `extra_headers` 目前无配置来源，保留为空。
    pub fn from_llm_settings(settings: &crate::config::LlmSettings) -> Self {
        let extras = if settings.wire_format.is_openai_compatible() {
            ProviderExtras::OpenAi(OpenAiProviderExtras {
                supports_prompt_cache_key: settings.supports_prompt_cache_key,
                supports_stream_usage: settings.supports_stream_usage,
                prompt_cache_retention: settings.prompt_cache_retention,
            })
        } else {
            ProviderExtras::None
        };
        Self {
            base_url: settings.base_url.clone(),
            api_key: settings.api_key.clone(),
            auth_scheme: settings.auth_scheme,
            connect_timeout_secs: settings.connect_timeout_secs,
            read_timeout_secs: settings.read_timeout_secs,
            max_retries: settings.max_retries,
            retry_base_delay_ms: settings.retry_base_delay_ms,
            supports_strict_tool_use: settings.supports_strict_tool_use,
            extras,
            extra_headers: std::collections::HashMap::new(),
            thinking: settings.thinking.clone(),
            thinking_capability: settings.thinking_capability.clone(),
            thinking_configured: settings.thinking_configured,
        }
    }

    fn openai_extras(&self) -> Option<&OpenAiProviderExtras> {
        match &self.extras {
            ProviderExtras::OpenAi(extras) => Some(extras),
            ProviderExtras::None => None,
        }
    }

    pub fn supports_prompt_cache_key(&self) -> bool {
        self.openai_extras()
            .is_some_and(|e| e.supports_prompt_cache_key)
    }

    pub fn supports_stream_usage(&self) -> bool {
        self.openai_extras()
            .is_some_and(|e| e.supports_stream_usage)
    }

    pub fn prompt_cache_retention(&self) -> Option<PromptCacheRetention> {
        self.openai_extras().and_then(|e| e.prompt_cache_retention)
    }
}

impl Default for LlmClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            api_key: String::new(),
            auth_scheme: crate::config::ProviderAuthScheme::Bearer,
            connect_timeout_secs: crate::config::defaults::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            read_timeout_secs: crate::config::defaults::DEFAULT_LLM_READ_TIMEOUT_SECS,
            max_retries: crate::config::defaults::DEFAULT_LLM_MAX_RETRIES,
            retry_base_delay_ms: crate::config::defaults::DEFAULT_LLM_RETRY_BASE_DELAY_MS,
            supports_strict_tool_use: false,
            extras: ProviderExtras::None,
            extra_headers: std::collections::HashMap::new(),
            thinking: ThinkingConfig::default(),
            thinking_capability: None,
            thinking_configured: false,
        }
    }
}

/// `LlmProvider` trait——所有 LLM 后端都实现此接口。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// 生成流式 LLM 响应。
    ///
    /// 返回一个通道接收端，按到达顺序产生 [`LlmEvent`] 值。
    /// 当流式输出完成或出错时通道关闭。
    async fn generate_request(
        &self,
        request: LlmRequest,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError>;

    /// 统计一次 provider request 的 input token。
    async fn count_input_tokens(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        Err(LlmError::Unsupported {
            message: "input token counting is not supported by this provider".into(),
        })
    }

    /// Provider 能接受的最小输出上限。
    fn minimum_output_tokens(&self) -> usize {
        1
    }

    /// 返回模型的上下文窗口限制。
    fn model_limits(&self) -> ModelLimits;
}

/// 一次运行时操作固定的主/小模型 provider 组合。
///
/// 这是进程内能力，不是 wire 或持久化契约。运行时边界显式传递该组合，避免配置发布前
/// 已开始的操作在执行中途静默切换 provider。
#[derive(Clone)]
pub struct LlmProviderBindings {
    main: Arc<dyn LlmProvider>,
    small: Arc<dyn LlmProvider>,
}

impl LlmProviderBindings {
    pub fn new(main: Arc<dyn LlmProvider>, small: Arc<dyn LlmProvider>) -> Self {
        Self { main, small }
    }

    pub fn main(&self) -> &Arc<dyn LlmProvider> {
        &self.main
    }

    pub fn small(&self) -> &Arc<dyn LlmProvider> {
        &self.small
    }
}

impl std::fmt::Debug for LlmProviderBindings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmProviderBindings")
            .field("main", &"<provider>")
            .field("small", &"<provider>")
            .finish()
    }
}

/// 模型的上下文窗口限制。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    /// 最大输入 token 数。
    pub max_input_tokens: usize,
    /// 最大输出 token 数。
    pub max_output_tokens: usize,
}

impl ModelLimits {
    /// 计算实际输出上限：请求值或模型上限，钳制到模型上限内且至少为 1。
    pub fn effective_output_cap(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.max_output_tokens)
            .min(self.max_output_tokens)
            .max(1)
    }
}

/// 从 LLM 事件流中收集所有文本增量，返回完整文本。
///
/// 遇到 `Error` 事件时返回错误，忽略非文本事件（tool call、thinking 等）。
pub async fn collect_stream_text(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LlmEvent>,
) -> Result<String, LlmError> {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => return Err(LlmError::stream_parse(message)),
            _ => {},
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_respects_inclusive_and_component_accounting() {
        let cases = [
            (
                LlmTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    output_tokens: Some(20),
                    total_tokens: Some(120),
                    input_accounting: Some(LlmInputTokenAccounting::Inclusive),
                    ..Default::default()
                },
                Some(100),
                Some(120),
            ),
            (
                LlmTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    cache_creation_input_tokens: Some(7),
                    output_tokens: Some(20),
                    input_accounting: Some(LlmInputTokenAccounting::Components),
                    ..Default::default()
                },
                Some(127),
                Some(147),
            ),
            (
                LlmTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    cache_creation_input_tokens: Some(7),
                    output_tokens: Some(20),
                    ..Default::default()
                },
                Some(127),
                Some(147),
            ),
            (
                LlmTokenUsage {
                    cached_input_tokens: Some(20),
                    total_tokens: Some(120),
                    ..Default::default()
                },
                Some(100),
                Some(120),
            ),
            (LlmTokenUsage::default(), None, None),
        ];

        for (usage, non_cached, context) in cases {
            assert_eq!(usage.non_cached_tokens(), non_cached, "usage: {usage:?}");
            assert_eq!(
                usage.context_tokens_after_response(),
                context,
                "usage: {usage:?}"
            );
        }
    }

    #[test]
    fn joined_text_preserves_text_blocks_and_ignores_other_content() {
        let mut message = LlmMessage::user("first");
        message.content.extend([
            LlmContent::Text {
                text: String::new(),
            },
            LlmContent::Image {
                base64: "image".into(),
                media_type: "image/png".into(),
                filename: None,
            },
            LlmContent::Text {
                text: "last".into(),
            },
        ]);

        assert_eq!(message.joined_text("|"), "first||last");
        assert_eq!(message.content[2].as_text(), None);
    }

    #[test]
    fn provider_visible_messages_truncates_dangling_tool_call_before_user_append() {
        let messages = vec![
            LlmMessage::user("start"),
            LlmMessage {
                role: LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: "call-1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "sleep"}),
                    raw_arguments: None,
                }],
                name: None,
                reasoning_content: None,
            },
            LlmMessage::user("next request after abort"),
        ];

        let visible = provider_visible_messages(messages);

        assert_eq!(visible, vec![LlmMessage::user("start")]);
    }

    #[test]
    fn provider_visible_filters_empty_system_messages() {
        let messages = vec![LlmMessage::user("hello"), LlmMessage::system("")];
        let visible = provider_visible_messages(messages);
        assert_eq!(visible.len(), 1);
        assert!(matches!(visible[0].role, LlmRole::User));
    }

    #[test]
    fn provider_visible_shared_matches_owned_and_never_writes_through() {
        let assistant_text = LlmMessage::assistant("text");
        let assistant_tool_call = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call-1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "sleep"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        };
        let tool_result = LlmMessage::tool("shell", "call-1", "ok", false);
        let messages = vec![
            LlmMessage::user("start"),
            assistant_text.clone(),
            assistant_tool_call.clone(),
            tool_result.clone(),
        ];
        let shared: Vec<Arc<LlmMessage>> = messages.iter().cloned().map(Arc::new).collect();

        let visible = provider_visible_shared_messages(shared.clone());

        // 与 owned 路径输出一致(assistant 文本并入带 tool call 的前一条)。
        assert_eq!(
            visible.iter().map(|m| (**m).clone()).collect::<Vec<_>>(),
            provider_visible_messages(messages)
        );
        // 合并走 copy-on-write:共享输入的消息体不被写穿。
        assert_eq!(*shared[1], assistant_text);
        assert_eq!(*shared[2], assistant_tool_call);
        assert!(Arc::ptr_eq(&shared[3], &visible[2]));
    }

    #[test]
    fn attachments_round_trip_preserves_image_filename() {
        let attachments = vec![MessageAttachment::image_png("screenshot.png", "abc123")];
        let message = LlmMessage::user_with_attachments("hello", &attachments);
        let round_trip = attachments_from_user_message(&message);
        assert_eq!(round_trip, attachments);
    }

    #[test]
    fn provider_visibility_attachment_framing_and_error_helpers_preserve_semantics() {
        let visible = provider_visible_messages(vec![
            LlmMessage::user("hello"),
            LlmMessage::assistant("world"),
        ]);
        assert_eq!(visible.len(), 2);

        let message = LlmMessage::user_with_attachments(
            "",
            &[MessageAttachment {
                filename: "note.txt".into(),
                content: "body".into(),
                media_type: "text/plain".into(),
            }],
        );
        let text = message
            .content
            .iter()
            .find_map(LlmContent::as_text)
            .expect("text attachment");
        assert!(text.starts_with("<attachment filename=\"note.txt\" media_type=\"text/plain\">"));
        assert!(text.ends_with("</attachment>"));

        assert!(matches!(
            LlmError::transport("boom"),
            LlmError::Transport { message } if message == "boom"
        ));
        assert!(matches!(
            LlmError::stream_parse("bad json"),
            LlmError::StreamParse { message } if message == "bad json"
        ));
    }

    #[test]
    fn is_retryable_classifies_correctly() {
        // 恒可重试
        assert!(
            LlmError::Transport {
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            LlmError::RateLimited {
                status: 429,
                retry_after_ms: None,
                message: "x".into(),
            }
            .is_retryable()
        );

        // ServerError 仅 408/500/502/503/504 可重试
        for retryable in [408, 500, 502, 503, 504] {
            assert!(
                LlmError::ServerError {
                    status: retryable,
                    message: "x".into(),
                }
                .is_retryable(),
                "{retryable} should be retryable"
            );
        }
        assert!(
            !LlmError::ServerError {
                status: 501,
                message: "x".into(),
            }
            .is_retryable()
        );

        // 其余变体不可重试
        assert!(
            !LlmError::InvalidApiKey {
                status: 401,
                message: "x".into(),
            }
            .is_retryable()
        );
        assert!(
            !LlmError::ContextWindowExceeded {
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            !LlmError::Unsupported {
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(!LlmError::Interrupted.is_retryable());
    }

    #[test]
    fn llm_error_serializes_with_snake_case_kind_tag() {
        // 内部 tag = "kind",变体名 snake_case;struct 字段内联。
        let json = serde_json::to_value(LlmError::ContextWindowExceeded {
            message: "too big".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], "context_window_exceeded");
        assert_eq!(json["message"], "too big");

        // RateLimited 省略 None 的 retry_after_ms
        let json = serde_json::to_value(LlmError::RateLimited {
            status: 429,
            retry_after_ms: None,
            message: "slow down".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], "rate_limited");
        assert_eq!(json["status"], 429);
        assert!(json.get("retry_after_ms").is_none());

        // retry_after_ms 有值时序列化
        let json = serde_json::to_value(LlmError::RateLimited {
            status: 429,
            retry_after_ms: Some(1500),
            message: "slow down".into(),
        })
        .unwrap();
        assert_eq!(json["retry_after_ms"], 1500);

        // unit 变体只带 tag
        let json = serde_json::to_value(LlmError::Interrupted).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "interrupted"}));
    }

    #[test]
    fn llm_error_round_trips_through_serde() {
        let cases: Vec<LlmError> = vec![
            LlmError::InvalidApiKey {
                status: 401,
                message: "bad key".into(),
            },
            LlmError::ModelNotFound {
                status: 404,
                message: "no model".into(),
            },
            LlmError::InvalidParameter {
                status: 400,
                message: "bad param".into(),
            },
            LlmError::QuotaExceeded {
                status: 402,
                message: "no funds".into(),
            },
            LlmError::ContextWindowExceeded {
                message: "too long".into(),
            },
            LlmError::RateLimited {
                status: 429,
                retry_after_ms: Some(2000),
                message: "rl".into(),
            },
            LlmError::ClientError {
                status: 418,
                message: "teapot".into(),
            },
            LlmError::ServerError {
                status: 503,
                message: "down".into(),
            },
            LlmError::Transport {
                message: "t".into(),
            },
            LlmError::StreamDisconnected {
                message: "d".into(),
            },
            LlmError::StreamParse {
                message: "p".into(),
            },
            LlmError::ContentFilter {
                message: "c".into(),
            },
            LlmError::TokenLimit {
                message: "l".into(),
            },
            LlmError::EmptyResponse,
            LlmError::Interrupted,
            LlmError::Unsupported {
                message: "u".into(),
            },
        ];
        for original in cases {
            let json = serde_json::to_string(&original).unwrap();
            let back: LlmError = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                json,
                "round-trip mismatch for {original:?}"
            );
        }
    }
}
