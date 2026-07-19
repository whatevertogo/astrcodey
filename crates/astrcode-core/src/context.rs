use std::{future::Future, path::PathBuf, pin::Pin};

use crate::{
    config::ContextSettings,
    llm::{LlmContent, LlmError, LlmMessage, LlmRole, ModelLimits},
    tool::ToolDefinition,
};

pub const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
pub const POST_COMPACT_CONTEXT_MARKER: &str = "<post_compact_context>";

/// 一次 provider request 的上下文准备输入。
///
/// `model_limits` 必须由调用方在每次请求前传入当前模型的限制，
/// 这样切换模型后 compact 阈值会立即跟随新窗口大小。
#[derive(Debug, Clone)]
pub struct ContextPrepareInput<'a> {
    /// 不包含 system prompt 的可见对话消息。
    pub messages: Vec<LlmMessage>,
    /// 已组装好的 system prompt；这里只参与 token 估算和 compact request。
    pub system_prompt: Option<&'a str>,
    /// 当前 provider/model 的上下文限制。
    pub model_limits: ModelLimits,
    /// provider 返回的 input token 统计；缺失时 context 层回退本地估算。
    pub provider_input_tokens: Option<usize>,
    /// 插件提供的 compact 指令，追加到 compact summary 中。
    pub custom_instructions: Vec<String>,
}

/// 已准备好的 provider 消息。
///
/// system prompt 不在这里返回；server 可以继续用自己的 system-message 前缀，
/// 这里负责返回 compact 后的可见消息窗口。
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<PreparedCompaction>,
}

#[derive(Debug, Clone)]
pub struct PreparedCompaction {
    pub result: CompactResult,
    pub llm_api_failed: bool,
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
    pub context_messages: Vec<LlmMessage>,
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

/// 是否执行 compact，以及 compact 时是否调用 LLM。
#[derive(Debug, Clone, Copy)]
pub struct CompactMessagesOptions {
    pub run: bool,
    pub use_llm: bool,
    pub keep_recent_turns: Option<usize>,
}

/// compact 执行结果（不含持久化）。
#[derive(Debug, Clone)]
pub enum CompactIfNeededOutcome {
    /// 未触发 compact（阈值未到且非 force）。
    NotRun { messages: Vec<LlmMessage> },
    /// 触发但无安全前缀可压（Empty / NothingToCompact）。
    Skipped { messages: Vec<LlmMessage> },
    /// 已生成摘要与新的可见窗口。
    Applied {
        messages: Vec<LlmMessage>,
        compaction: PreparedCompaction,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PrepareMessagesOptions {
    /// 根据阈值或 force 执行 compact（与是否调用 LLM 无关）。
    pub run_compact: bool,
    /// compact 时是否调用 LLM；为 false 时仅用确定性模板。
    pub use_llm_for_compact: bool,
    pub force_compact: bool,
    pub keep_recent_turns: Option<usize>,
}

pub type CompactRequestFn = Box<
    dyn FnMut(Vec<LlmMessage>) -> Pin<Box<dyn Future<Output = Result<String, CompactError>> + Send>>
        + Send,
>;

#[async_trait::async_trait]
pub trait ContextAssembler: Send + Sync {
    fn settings(&self) -> &ContextSettings;

    fn auto_compact_enabled(&self) -> bool {
        self.settings().auto_compact_enabled
    }

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool;

    async fn compact_if_needed(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        custom_instructions: &[String],
        render_options: CompactSummaryRenderOptions,
        options: CompactMessagesOptions,
        request_text: CompactRequestFn,
    ) -> CompactIfNeededOutcome;
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
