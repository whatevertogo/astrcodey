//! LLM 提供者抽象与消息类型。
//!
//! 本模块定义了与 LLM 交互所需的核心类型：
//! - [`LlmMessage`] / [`LlmContent`]：对话消息和内容类型
//! - [`LlmEvent`]：LLM 流式输出事件
//! - [`LlmProvider`] trait：所有 LLM 后端的统一接口
//! - [`LlmClientConfig`]：LLM 客户端配置
//! - [`LlmError`]：LLM 操作错误类型

use serde::{Deserialize, Serialize};

use crate::{message_attachment::MessageAttachment, tool::ToolDefinition};

/// LLM 对话消息中的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 将内容转换为人类可读的纯文本展示。
    ///
    /// 这是有损转换——不可能完全还原原始渲染效果。
    /// - `Text` / `ToolResult`：原样输出。
    /// - `Image`：返回占位符 `[image]`。
    /// - `ToolCall`：大多数工具调用只输出工具名；`upsertSessionPlan` 额外提取 arguments.content
    ///   中的 plan 正文。
    pub fn to_display_text(&self) -> String {
        match self {
            LlmContent::Text { text } => text.clone(),
            LlmContent::Image { .. } => "[image]".into(),
            LlmContent::ToolCall {
                name, arguments, ..
            } => match name.as_str() {
                "upsertSessionPlan" => arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                _ => format!("tool call: {name}"),
            },
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

    /// 由文本与附件构建用户消息（图片走 [`LlmContent::Image`]）。
    pub fn user_with_attachments(text: &str, attachments: &[MessageAttachment]) -> Self {
        user_message_with_attachments(text, attachments)
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

    /// 返回 provider 可见版本，保留需要回传给 provider 的字段（如 reasoning_content）。
    pub fn provider_visible(self) -> Self {
        self
    }

    /// 将各 content 块经 [`LlmContent::to_display_text`] 转换后用 `separator` 拼接。
    pub fn joined_display_text(&self, separator: &str) -> String {
        self.content
            .iter()
            .map(LlmContent::to_display_text)
            .collect::<Vec<_>>()
            .join(separator)
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

/// 从用户 LLM 消息中提取附件（与 [`user_message_with_attachments`] 对称）。
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

/// 将用户文本与附件组装为 LLM 用户消息。
pub fn user_message_with_attachments(text: &str, attachments: &[MessageAttachment]) -> LlmMessage {
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
    LlmMessage {
        role: LlmRole::User,
        content,
        name: None,
        reasoning_content: None,
    }
}

pub const TURN_ABORTED_SOURCE: &str = "turn_aborted";
pub const TURN_ABORTED_GUIDANCE: &str = concat!(
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
    let mut messages = messages
        .into_iter()
        .map(LlmMessage::provider_visible)
        .filter(LlmMessage::has_provider_visible_content)
        .collect::<Vec<_>>();
    normalize_tool_call_messages(&mut messages);
    truncate_incomplete_tool_protocol(&mut messages);
    messages
}

fn normalize_tool_call_messages(messages: &mut Vec<LlmMessage>) {
    let mut merged: Vec<LlmMessage> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        let has_tool_calls = message.role == LlmRole::Assistant
            && message
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. }));
        if has_tool_calls {
            if let Some(last) = merged.last_mut() {
                if last.role == LlmRole::Assistant {
                    last.content.extend(message.content);
                    if last.reasoning_content.is_none() {
                        last.reasoning_content = message.reasoning_content;
                    }
                    continue;
                }
            }
        }
        merged.push(message);
    }
    *messages = merged;
}

fn truncate_incomplete_tool_protocol(messages: &mut Vec<LlmMessage>) {
    use std::collections::HashSet;

    let mut pending: Option<(usize, HashSet<String>, HashSet<String>)> = None;

    for index in 0..messages.len() {
        let message = &messages[index];
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LlmTokenUsageSource>,
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

/// LLM 流式输出过程中的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmEvent {
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
    /// 单次 LLM 调用的 token 使用统计。
    Usage { usage: LlmTokenUsage },
    /// 流式输出已完成。
    Done { finish_reason: String },
    /// 流式输出过程中发生错误。
    Error { message: String },
}

/// LLM 调用的完整输出结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOutput {
    /// 累积的文本内容。
    pub text: String,
    /// LLM 请求的工具调用列表（如有）。
    pub tool_calls: Vec<ParsedToolCall>,
    /// 提供者返回的完成原因。
    pub finish_reason: String,
}

/// 从 LLM 响应中解析出的工具调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedToolCall {
    /// 工具调用的唯一标识。
    pub call_id: String,
    /// 要调用的工具名称。
    pub name: String,
    /// 工具调用参数（JSON 值）。
    pub arguments: serde_json::Value,
}

/// LLM 提供者操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 提示词超出模型上下文长度限制。
    #[error("Prompt too long: {0}")]
    PromptTooLong(String),
    /// 客户端错误（4xx 状态码）。
    #[error("Client error ({status}): {message}")]
    ClientError { status: u16, message: String },
    /// 服务端错误（5xx 状态码）。
    #[error("Server error ({status}): {message}")]
    ServerError { status: u16, message: String },
    /// 网络传输错误。
    #[error("Transport error: {0}")]
    Transport(String),
    /// 请求被中断（用户取消）。
    #[error("Request interrupted")]
    Interrupted,
    /// 流式响应解析错误。
    #[error("Stream parse error: {0}")]
    StreamParse(String),
    /// 当前 provider 不支持该操作。
    #[error("Unsupported LLM operation: {0}")]
    Unsupported(String),
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

impl PromptCacheRetention {
    /// 返回 OpenAI 兼容请求体中的 wire 值。
    pub fn as_wire_value(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::TwentyFourHours => "24h",
        }
    }
}

/// 推理强度级别（跨模型选项的标准化抽象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    /// 返回 OpenAI Responses `reasoning.effort` 的 wire 值。
    pub fn as_wire_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// OpenAI 兼容 API 的 provider 特有选项（prompt cache、thinking level 等）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAiProviderExtras {
    /// 当前 provider 是否支持 OpenAI `prompt_cache_key`。
    pub supports_prompt_cache_key: bool,
    /// 当前 provider 是否支持流式 usage 统计。
    pub supports_stream_usage: bool,
    /// 可选的 OpenAI prompt cache retention。
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    /// 可选的推理强度（用于 OpenAI Responses `reasoning.effort`）。
    pub thinking_level: Option<ThinkingLevel>,
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
    /// 连接超时时间（秒）。
    pub connect_timeout_secs: u64,
    /// 读取超时时间（秒）。
    pub read_timeout_secs: u64,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 指数退避的基础延迟（毫秒）。
    pub retry_base_delay_ms: u64,
    /// 当前模型是否为 reasoning/thinking 模式。
    pub reasoning: bool,
    /// Provider 特有选项。
    pub extras: ProviderExtras,
    /// 额外的 HTTP 请求头。
    pub extra_headers: std::collections::HashMap<String, String>,
}

impl LlmClientConfig {
    pub fn openai_extras(&self) -> Option<&OpenAiProviderExtras> {
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

    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.openai_extras().and_then(|e| e.thinking_level)
    }
}

impl Default for LlmClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            api_key: String::new(),
            connect_timeout_secs: 10,
            read_timeout_secs: 90,
            max_retries: 2,
            retry_base_delay_ms: 250,
            reasoning: false,
            extras: ProviderExtras::None,
            extra_headers: std::collections::HashMap::new(),
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
    ///
    /// - `messages`：对话消息列表
    /// - `tools`：可供 LLM 调用的工具定义列表
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError>;

    /// 统计一次 provider request 的 input token。
    async fn count_input_tokens(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        Err(LlmError::Unsupported(
            "input token counting is not supported by this provider".into(),
        ))
    }

    /// 返回模型的上下文窗口限制。
    fn model_limits(&self) -> ModelLimits;
}

/// 模型的上下文窗口限制。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    /// 最大输入 token 数。
    pub max_input_tokens: usize,
    /// 最大输出 token 数。
    pub max_output_tokens: usize,
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
            LlmEvent::Error { message } => return Err(LlmError::StreamParse(message)),
            _ => {},
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn attachments_round_trip_preserves_image_filename() {
        let attachments = vec![MessageAttachment::image_png("screenshot.png", "abc123")];
        let message = user_message_with_attachments("hello", &attachments);
        let round_trip = attachments_from_user_message(&message);
        assert_eq!(round_trip, attachments);
    }

    #[test]
    fn non_image_attachment_uses_xml_delimiters() {
        let attachments = vec![MessageAttachment {
            filename: "note.txt".into(),
            content: "body".into(),
            media_type: "text/plain".into(),
        }];
        let message = user_message_with_attachments("", &attachments);
        let text = message
            .content
            .iter()
            .find_map(|part| match part {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("text attachment");
        assert!(text.starts_with("<attachment filename=\"note.txt\" media_type=\"text/plain\">"));
        assert!(text.ends_with("</attachment>"));
    }
}
