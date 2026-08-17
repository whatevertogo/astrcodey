use std::sync::Arc;

use astrcode_core::llm::{
    LlmError, LlmMessage, SharedTranscriptMessage, TranscriptMessage, TranscriptMessageOrigin,
    provider_transcript_messages, provider_transcript_shared_messages,
    provider_visible_shared_messages, token_estimate::estimate_provider_message_tokens,
};

use crate::prompt_engine::system_messages_from_prompt;

/// 同一 durable revision 下的完整 provider context。
///
/// Compact candidate 从该 snapshot 生成；提交时 `source_seq` 用于保留之后到达的 transcript tail。
///
/// `origins` 与 `messages` 等长平行存储,避免 transcript 元数据导致消息双份存放。
///
/// `messages` 经 `Arc` 与读模型共享,构造后不可变;请求组装只 clone `Arc` 指针,
/// 不复制消息体。
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub source_seq: u64,
    pub system_prompt: String,
    pub messages: Vec<Arc<LlmMessage>>,
    origins: Vec<Option<TranscriptMessageOrigin>>,
    input_token_anchor: Option<InputTokenAnchor>,
}

#[derive(Debug, Clone)]
struct InputTokenAnchor {
    context_tokens: usize,
    model_context_window: usize,
    covered_message_count: usize,
}

impl ContextSnapshot {
    pub fn new(source_seq: u64, system_prompt: String, messages: Vec<LlmMessage>) -> Self {
        Self::from_transcript(
            source_seq,
            system_prompt,
            messages.into_iter().map(TranscriptMessage::plain).collect(),
        )
    }

    pub fn from_transcript(
        source_seq: u64,
        system_prompt: String,
        messages: Vec<TranscriptMessage>,
    ) -> Self {
        let (messages, origins) = provider_transcript_messages(messages)
            .into_iter()
            .map(|entry| (Arc::new(entry.message), entry.origin))
            .unzip();
        Self {
            source_seq,
            system_prompt,
            messages,
            origins,
            input_token_anchor: None,
        }
    }

    /// 从读模型的共享 transcript 构建;与 [`Self::from_transcript`] 走同一归一化,
    /// 消息体经 `Arc` 零拷贝复用。
    pub fn from_shared_transcript(
        source_seq: u64,
        system_prompt: String,
        messages: Vec<SharedTranscriptMessage>,
    ) -> Self {
        let (messages, origins) = provider_transcript_shared_messages(messages)
            .into_iter()
            .map(|entry| (entry.message, entry.origin))
            .unzip();
        Self {
            source_seq,
            system_prompt,
            messages,
            origins,
            input_token_anchor: None,
        }
    }

    /// 返回 compact 原样保留的 provider transcript 尾部及其 durable 元数据。
    pub fn retained_transcript_messages(
        &self,
        retained_messages: &[LlmMessage],
    ) -> Option<Vec<TranscriptMessage>> {
        let start = self.messages.len().checked_sub(retained_messages.len())?;
        if self.messages[start..]
            .iter()
            .map(|message| message.as_ref())
            .ne(retained_messages.iter())
        {
            return None;
        }
        Some(
            self.messages[start..]
                .iter()
                .zip(&self.origins[start..])
                .map(|(message, origin)| TranscriptMessage {
                    message: (**message).clone(),
                    origin: *origin,
                })
                .collect(),
        )
    }

    /// 绑定 provider usage 覆盖的 transcript 前缀。
    pub fn with_input_token_anchor(
        mut self,
        context_tokens: usize,
        model_context_window: usize,
        covered_message_count: usize,
    ) -> Self {
        if covered_message_count <= self.messages.len() {
            self.input_token_anchor = Some(InputTokenAnchor {
                context_tokens,
                model_context_window,
                covered_message_count,
            });
        }
        self
    }

    /// 将可见 transcript 与当前 system prompt 组装为完整 provider 请求。
    pub fn request_messages(&self, messages: Vec<Arc<LlmMessage>>) -> Vec<Arc<LlmMessage>> {
        let mut request = Vec::with_capacity(messages.len().saturating_add(4));
        request.extend(
            system_messages_from_prompt(&self.system_prompt)
                .into_iter()
                .map(Arc::new),
        );
        request.extend(messages);
        provider_visible_shared_messages(request)
    }

    /// 估算由本 snapshot 直接组装的请求的输入 token,不物化请求 Vec。
    ///
    /// 与 [`Self::estimate_input_tokens`] 的锚点快路径等价:请求由 snapshot
    /// 自身构建时前缀必然匹配,跳过了物化后的前缀验证。
    ///
    /// `tools_tokens` 由调用方按可见工具集 memo 后传入(由
    /// `estimate_tool_definition_tokens` 预先计算)。
    pub fn estimate_own_input_tokens(
        &self,
        tools_tokens: usize,
        model_context_window: usize,
    ) -> usize {
        let full_estimate = || {
            estimate_provider_message_tokens(
                system_messages_from_prompt(&self.system_prompt)
                    .iter()
                    .chain(self.messages.iter().map(|message| message.as_ref())),
            )
            .saturating_add(tools_tokens)
        };
        let Some(anchor) = &self.input_token_anchor else {
            return full_estimate();
        };
        if anchor.model_context_window != model_context_window {
            return full_estimate();
        }
        let Some(trailing_messages) = self.messages.get(anchor.covered_message_count..) else {
            return full_estimate();
        };
        anchor
            .context_tokens
            .saturating_add(estimate_provider_message_tokens(
                trailing_messages.iter().map(|message| message.as_ref()),
            ))
            // 与 estimate_input_tokens 相同:再次计入工具是有意的保守上界。
            .saturating_add(tools_tokens)
    }

    /// 估算最终 provider 请求输入。优先复用最近 provider usage，仅估算新增尾部。
    pub fn estimate_input_tokens(
        &self,
        request_messages: &[Arc<LlmMessage>],
        tools_tokens: usize,
        model_context_window: usize,
    ) -> usize {
        let full_estimate = || {
            estimate_provider_message_tokens(
                request_messages.iter().map(|message| message.as_ref()),
            )
            .saturating_add(tools_tokens)
        };
        let Some(anchor) = &self.input_token_anchor else {
            return full_estimate();
        };
        if anchor.model_context_window != model_context_window {
            return full_estimate();
        }
        let system_messages = system_messages_from_prompt(&self.system_prompt);
        let Some(covered_messages) = self.messages.get(..anchor.covered_message_count) else {
            return full_estimate();
        };
        let prefix_len = system_messages.len().saturating_add(covered_messages.len());
        let Some(request_prefix) = request_messages.get(..prefix_len) else {
            return full_estimate();
        };
        if !request_prefix[..system_messages.len()]
            .iter()
            .map(|message| message.as_ref())
            .eq(system_messages.iter())
            || !request_prefix[system_messages.len()..]
                .iter()
                .eq(covered_messages.iter())
        {
            return full_estimate();
        }
        let trailing_messages = &request_messages[prefix_len..];

        anchor
            .context_tokens
            .saturating_add(estimate_provider_message_tokens(
                trailing_messages.iter().map(|message| message.as_ref()),
            ))
            // Provider usage 已包含上一请求的工具；再次计入当前工具是有意的保守上界，
            // 同时覆盖 turn 中 deferred tool 激活或工具目录热更新。
            .saturating_add(tools_tokens)
    }
}

/// compact summary 渲染选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactSummaryRenderOptions {
    pub transcript_path: Option<String>,
    pub custom_instructions: Vec<String>,
}

/// Extension-owned context retained alongside a compact summary.
///
/// The context crate intentionally treats both variants as opaque content. Extension-specific
/// discovery and freshness rules are applied before values cross into this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactRetainedContext {
    File { path: String, content: String },
    Note { title: String, body: String },
}

impl CompactRetainedContext {
    pub(crate) fn estimated_tokens(&self) -> usize {
        let (label, content) = match self {
            Self::File { path, content } => (path, content),
            Self::Note { title, body } => (title, body),
        };
        crate::token_budget::estimate_text_tokens(label)
            .saturating_add(crate::token_budget::estimate_text_tokens(content))
    }
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

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::token_estimate::{estimate_provider_request_tokens, estimate_tool_definition_tokens},
        tool::{ToolDefinition, ToolOrigin},
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn input_estimate_reuses_only_matching_provider_usage_prefix() {
        let shared = |messages: Vec<LlmMessage>| messages.into_iter().map(Arc::new).collect();
        let owned = |messages: &[Arc<LlmMessage>]| {
            messages.iter().map(|m| (**m).clone()).collect::<Vec<_>>()
        };
        let covered_messages = vec![LlmMessage::user("first"), LlmMessage::assistant("response")];
        let snapshot = ContextSnapshot::new(2, "system".into(), covered_messages)
            .with_input_token_anchor(655_859, 1_000_000, 2);
        let request_messages = snapshot.request_messages(shared(vec![
            LlmMessage::user("first"),
            LlmMessage::assistant("response"),
            LlmMessage::user("tail"),
        ]));
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
            strict: false,
            origin: ToolOrigin::Bundled,
        }];
        let tools_tokens = estimate_tool_definition_tokens(&tools);

        let anchored = snapshot.estimate_input_tokens(&request_messages, tools_tokens, 1_000_000);
        let local = estimate_provider_request_tokens(&owned(&request_messages), &tools);
        assert!(anchored > 655_859);
        assert!(anchored > local);
        assert_eq!(
            snapshot.estimate_input_tokens(&request_messages, tools_tokens, 200_000),
            local
        );

        let changed_prefix = snapshot.request_messages(shared(vec![LlmMessage::user("changed")]));
        assert_eq!(
            snapshot.estimate_input_tokens(&changed_prefix, tools_tokens, 1_000_000),
            estimate_provider_request_tokens(&owned(&changed_prefix), &tools)
        );
    }
}
