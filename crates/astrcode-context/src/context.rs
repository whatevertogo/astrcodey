use std::path::PathBuf;

use astrcode_core::{
    config::ContextSettings,
    llm::{LlmContent, LlmError, LlmMessage, LlmRole, provider_visible_messages},
    tool::ToolDefinition,
};

use crate::prompt_engine::system_messages_from_prompt;

pub const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
pub const POST_COMPACT_CONTEXT_MARKER: &str = "<post_compact_context>";

/// 同一 durable revision 下的完整 provider context。
///
/// Compact candidate 从该 snapshot 生成；提交时 `source_seq` 用于保留之后到达的 transcript tail。
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub source_seq: u64,
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
}

impl ContextSnapshot {
    pub fn new(source_seq: u64, system_prompt: String, messages: Vec<LlmMessage>) -> Self {
        let mut messages = provider_visible_messages(messages);
        messages.retain(|message| message.role != LlmRole::System);
        Self {
            source_seq,
            system_prompt,
            messages,
        }
    }

    /// 将可见 transcript 与当前 system prompt 组装为完整 provider 请求。
    pub fn request_messages(&self, messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
        let mut request = Vec::with_capacity(messages.len().saturating_add(4));
        request.extend(system_messages_from_prompt(&self.system_prompt));
        request.extend(messages);
        provider_visible_messages(request)
    }
}

/// compact summary 渲染选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactSummaryRenderOptions {
    pub transcript_path: Option<String>,
    pub custom_instructions: Vec<String>,
}

/// 压缩操作的结果。
///
/// 记录压缩前后的 token 数量以及生成的摘要文本。
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// 压缩前的 token 数量。
    pub pre_tokens: usize,
    /// 压缩后的 token 数量。
    pub post_tokens: usize,
    /// 生成的对话摘要。
    pub summary: String,
    /// 压缩掉的可见消息数量。
    pub messages_removed: usize,
    /// 供 provider 使用的合成上下文消息。
    pub summary_messages: Vec<LlmMessage>,
    /// 保留的可见消息尾部。
    pub retained_messages: Vec<LlmMessage>,
    /// compact 前 transcript snapshot 的可读路径。
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactSkipReason {
    /// 没有任何可压缩消息。
    Empty,
    /// 有消息，但根据当前切分策略没有安全的历史前缀可压缩。
    NothingToCompact,
}

#[derive(Debug)]
pub enum CompactError {
    Skip(CompactSkipReason),
    Parse(String),
    Llm(LlmError),
}

impl std::fmt::Display for CompactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skip(reason) => write!(f, "compact skipped: {reason:?}"),
            Self::Parse(error) => write!(f, "compact parse error: {error}"),
            Self::Llm(error) => write!(f, "compact llm error: {error}"),
        }
    }
}

impl std::error::Error for CompactError {}

impl From<CompactSkipReason> for CompactError {
    fn from(value: CompactSkipReason) -> Self {
        Self::Skip(value)
    }
}

impl From<LlmError> for CompactError {
    fn from(value: LlmError) -> Self {
        Self::Llm(value)
    }
}

/// 判断消息是否是 compact 后注入的 synthetic context message。
pub fn is_compact_summary_message(message: &LlmMessage) -> bool {
    message.role == LlmRole::User
        && message
            .content
            .iter()
            .filter_map(LlmContent::as_text)
            .any(is_compact_summary_text)
}

/// 检测文本内容是否以 compact summary 标记开头。
pub fn is_compact_summary_text(content: &str) -> bool {
    content.trim_start().starts_with(COMPACT_SUMMARY_MARKER)
}

/// 判断消息是否是 compact/post-compact 注入的 synthetic context message。
pub fn is_synthetic_context_message(message: &LlmMessage) -> bool {
    is_compact_summary_message(message)
        || (message.role == LlmRole::User
            && message
                .content
                .iter()
                .filter_map(LlmContent::as_text)
                .any(|text| text.trim_start().starts_with(POST_COMPACT_CONTEXT_MARKER)))
}

/// 粗略识别 provider 返回的上下文过长错误。
///
/// 这里故意排除 rate limit / quota 等错误，避免把限流误判为可 compact 重试。
pub fn is_prompt_too_long_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let positive = [
        "prompt too long",
        "context length",
        "maximum context",
        "too many tokens",
        "input is too long",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let negative = ["rate limit", "quota", "throttle", "timeout"]
        .iter()
        .any(|needle| lower.contains(needle));
    positive && !negative
}

pub struct PostCompactEnrichInput<'a> {
    pub session_id: &'a str,
    pub source_messages: &'a [LlmMessage],
    pub working_dir: &'a str,
    pub system_prompt: Option<&'a str>,
    pub tools: &'a [ToolDefinition],
    pub settings: &'a ContextSettings,
    pub session_store_dir: Option<PathBuf>,
}

#[async_trait::async_trait]
pub trait PostCompactEnricher: Send + Sync {
    async fn enrich(&self, compaction: &mut CompactResult, input: PostCompactEnrichInput<'_>);
}

pub struct NoopPostCompactEnricher;

#[async_trait::async_trait]
impl PostCompactEnricher for NoopPostCompactEnricher {
    async fn enrich(&self, _compaction: &mut CompactResult, _input: PostCompactEnrichInput<'_>) {}
}
