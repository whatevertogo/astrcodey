//! Compact pipeline — hook 桥接 + forked provider 调用。
//!
//! 包含：
//! - [`CompactHookContext`]：在 compact 前后触发扩展钩子所需的上下文
//! - [`compact_with_forked_provider`]：使用独立 LLM 调用执行 compact
//!
//! 设计约束：这个模块不持有任何会话状态，所有参数通过调用方传入。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use astrcode_context::{
    ContextSettings,
    compaction::{
        CompactError, CompactResult, CompactSkipReason, CompactSummaryRenderOptions,
        compact_messages_with_request,
    },
};
use astrcode_core::{
    config::ModelSelection,
    extension::{
        CompactTrigger, ExtensionError, ExtensionEvent, PostCompactInput, PreCompactInput,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider},
    tool::ToolDefinition,
    types::SessionId,
};
use astrcode_extensions::{context::ServerExtensionContext, runner::ExtensionRunner};

pub const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: usize = 3;

/// Runtime-local fuse for repeated provider-backed auto compact failures.
///
/// The count follows compact continuation children so one failing session line
/// does not pay for the same broken summary request every turn.
#[derive(Debug, Default)]
pub struct AutoCompactFailureTracker {
    counts: Mutex<HashMap<SessionId, usize>>,
}

impl AutoCompactFailureTracker {
    pub(crate) fn should_skip_provider(&self, session_id: &SessionId) -> bool {
        self.consecutive_failures(session_id) >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
    }

    pub(crate) fn consecutive_failures(&self, session_id: &SessionId) -> usize {
        *self.counts().get(session_id).unwrap_or(&0)
    }

    pub(crate) fn record_provider_success(&self, session_id: &SessionId) {
        self.counts().remove(session_id);
    }

    pub(crate) fn record_provider_failure(&self, session_id: &SessionId) -> usize {
        let mut counts = self.counts();
        let count = counts.entry(session_id.clone()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    fn counts(&self) -> MutexGuard<'_, HashMap<SessionId, usize>> {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ─── Hook 上下文 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub(crate) struct CompactHookContext<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) working_dir: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) trigger: CompactTrigger,
    pub(crate) message_count: usize,
}

pub(crate) async fn collect_compact_instructions(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
) -> Result<Vec<String>, ExtensionError> {
    let mut ctx = compact_extension_context(&input);
    ctx.set_pre_compact_input(PreCompactInput {
        trigger: input.trigger,
        message_count: input.message_count,
    });

    let contributions = extension_runner.collect_compact_contributions(&ctx).await?;
    Ok(clean_compact_instructions(contributions.instructions))
}

pub(crate) async fn dispatch_post_compact(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    let mut ctx = compact_extension_context(&input);
    ctx.set_post_compact_input(PostCompactInput {
        trigger: input.trigger,
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        messages_removed: compaction.messages_removed,
        summary: compaction.summary.clone(),
    });
    extension_runner
        .dispatch(ExtensionEvent::PostCompact, &ctx)
        .await
}

pub(crate) fn compact_trigger_name(trigger: CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::AutoThreshold => "auto_threshold",
        CompactTrigger::ManualCommand => "manual_command",
    }
}

fn compact_extension_context(input: &CompactHookContext<'_>) -> ServerExtensionContext {
    let mut ctx = ServerExtensionContext::new(
        input.session_id.to_string(),
        input.working_dir.to_string(),
        ModelSelection::simple(input.model_id),
    );
    ctx.set_tools(
        input
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.name.clone(), tool))
            .collect(),
    );
    ctx
}

fn clean_compact_instructions(instructions: Vec<String>) -> Vec<String> {
    instructions
        .into_iter()
        .map(|instruction| instruction.trim().to_string())
        .filter(|instruction| !instruction.is_empty())
        .collect()
}

// ─── Forked provider ─────────────────────────────────────────────────────

pub(crate) struct ForkedProviderRequest {
    pub(crate) base_messages: Vec<LlmMessage>,
    pub(crate) prompt_messages: Vec<LlmMessage>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) max_turns: usize,
}

#[derive(Debug)]
pub(crate) struct ForkedProviderOutput {
    pub(crate) text: String,
    pub(crate) finish_reason: String,
}

#[derive(Debug)]
pub(crate) enum ForkedRunError {
    Llm(LlmError),
    UnexpectedToolCall { name: String },
    UnsupportedMaxTurns { max_turns: usize },
}

impl ForkedRunError {
    fn into_llm_error(self) -> LlmError {
        match self {
            Self::Llm(error) => error,
            Self::UnexpectedToolCall { name } => {
                LlmError::StreamParse(format!("forked compact attempted to call tool '{name}'"))
            },
            Self::UnsupportedMaxTurns { max_turns } => LlmError::StreamParse(format!(
                "forked provider runner only supports max_turns = 1, got {max_turns}"
            )),
        }
    }
}

impl From<LlmError> for ForkedRunError {
    fn from(value: LlmError) -> Self {
        Self::Llm(value)
    }
}

#[derive(Clone)]
pub(crate) struct ForkedProviderRunner {
    llm: Arc<dyn LlmProvider>,
}

impl ForkedProviderRunner {
    pub(crate) fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self { llm }
    }

    pub(crate) async fn run_one_turn(
        &self,
        request: ForkedProviderRequest,
    ) -> Result<ForkedProviderOutput, ForkedRunError> {
        if request.max_turns != 1 {
            return Err(ForkedRunError::UnsupportedMaxTurns {
                max_turns: request.max_turns,
            });
        }

        let mut messages = request.base_messages;
        messages.extend(request.prompt_messages);
        let mut rx = self.llm.generate(messages, request.tools).await?;
        let mut text = String::new();

        while let Some(event) = rx.recv().await {
            match event {
                LlmEvent::ContentDelta { delta } => text.push_str(&delta),
                LlmEvent::ThinkingDelta { delta } => text.push_str(&delta),
                LlmEvent::Done { finish_reason } => {
                    return Ok(ForkedProviderOutput {
                        text,
                        finish_reason,
                    });
                },
                LlmEvent::Error { message } => {
                    return Err(ForkedRunError::Llm(LlmError::StreamParse(message)));
                },
                LlmEvent::ToolCallStart { name, .. } => {
                    // TODO: 未来可能通过 hook/policy 层限制 forked compact 的工具调用。
                    return Err(ForkedRunError::UnexpectedToolCall { name });
                },
                LlmEvent::ToolCallDelta { .. } => {
                    return Err(ForkedRunError::UnexpectedToolCall {
                        name: "unknown".into(),
                    });
                },
            }
        }

        Ok(ForkedProviderOutput {
            text,
            finish_reason: "stream_closed".into(),
        })
    }
}

#[derive(Clone)]
struct CompactForkRunner {
    forked: ForkedProviderRunner,
    tools: Vec<ToolDefinition>,
}

impl CompactForkRunner {
    fn new(llm: Arc<dyn LlmProvider>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            forked: ForkedProviderRunner::new(llm),
            tools,
        }
    }

    async fn run_compact_request(&self, messages: Vec<LlmMessage>) -> Result<String, CompactError> {
        self.forked
            .run_one_turn(ForkedProviderRequest {
                base_messages: messages,
                prompt_messages: Vec::new(),
                tools: self.tools.clone(),
                max_turns: 1,
            })
            .await
            .map(|output| {
                let _finish_reason = output.finish_reason;
                output.text
            })
            .map_err(|error| CompactError::Llm(error.into_llm_error()))
    }
}

pub(crate) async fn compact_with_forked_provider(
    llm: Arc<dyn LlmProvider>,
    tools: Vec<ToolDefinition>,
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    settings: &ContextSettings,
    custom_instructions: &[String],
    render_options: &CompactSummaryRenderOptions,
) -> Result<CompactResult, CompactError> {
    let runner = CompactForkRunner::new(llm, tools);
    compact_messages_with_request(
        messages,
        system_prompt,
        settings,
        custom_instructions,
        render_options,
        move |request_messages| {
            let runner = runner.clone();
            async move { runner.run_compact_request(request_messages).await }
        },
    )
    .await
}

/// 把 compact 结果转换成主循环继续发送给 provider 的 prepared context。
pub(crate) fn prepared_context_from_compaction(
    compaction: CompactResult,
) -> astrcode_context::manager::PreparedContext {
    let messages = [
        compaction.context_messages.clone(),
        compaction.retained_messages.clone(),
    ]
    .concat();
    astrcode_context::manager::PreparedContext {
        messages,
        compaction: Some(compaction),
    }
}

pub(crate) fn counts_as_auto_compact_provider_failure(error: &CompactError) -> bool {
    !matches!(
        error,
        CompactError::Skip(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact)
    )
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::ModelLimits;

    use super::*;

    struct ToolCallingLlm;

    #[async_trait::async_trait]
    impl LlmProvider for ToolCallingLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ToolCallStart {
                call_id: "compact-call".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "tool_calls".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct StreamErrorLlm;

    #[async_trait::async_trait]
    impl LlmProvider for StreamErrorLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::Error {
                message: "provider crashed".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct DanglingStreamCompactLlm;

    #[async_trait::async_trait]
    impl LlmProvider for DanglingStreamCompactLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "partial".into(),
            });
            // Drop sender without Done — simulates stream close
            drop(tx);
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    #[tokio::test]
    async fn forked_runner_rejects_unexpected_tool_call() {
        let runner = ForkedProviderRunner::new(Arc::new(ToolCallingLlm));
        let result = runner
            .run_one_turn(ForkedProviderRequest {
                base_messages: vec![LlmMessage::user("compact")],
                prompt_messages: vec![],
                tools: vec![],
                max_turns: 1,
            })
            .await;

        match result {
            Err(ForkedRunError::UnexpectedToolCall { name }) => {
                assert_eq!(name, "shell");
            },
            other => panic!("expected UnexpectedToolCall, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn forked_runner_propagates_stream_error() {
        let runner = ForkedProviderRunner::new(Arc::new(StreamErrorLlm));
        let result = runner
            .run_one_turn(ForkedProviderRequest {
                base_messages: vec![LlmMessage::user("compact")],
                prompt_messages: vec![],
                tools: vec![],
                max_turns: 1,
            })
            .await;

        match result {
            Err(ForkedRunError::Llm(LlmError::StreamParse(msg))) => {
                assert!(msg.contains("provider crashed"));
            },
            other => panic!("expected StreamParse error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn forked_runner_returns_stream_closed_when_sender_drops() {
        let runner = ForkedProviderRunner::new(Arc::new(DanglingStreamCompactLlm));
        let result = runner
            .run_one_turn(ForkedProviderRequest {
                base_messages: vec![LlmMessage::user("compact")],
                prompt_messages: vec![],
                tools: vec![],
                max_turns: 1,
            })
            .await
            .expect("dangling stream should return Ok with stream_closed");

        assert_eq!(result.finish_reason, "stream_closed");
        assert_eq!(result.text, "partial");
    }

    #[tokio::test]
    async fn forked_runner_rejects_unsupported_max_turns() {
        let runner = ForkedProviderRunner::new(Arc::new(StreamErrorLlm));
        let result = runner
            .run_one_turn(ForkedProviderRequest {
                base_messages: vec![],
                prompt_messages: vec![],
                tools: vec![],
                max_turns: 3,
            })
            .await;

        match result {
            Err(ForkedRunError::UnsupportedMaxTurns { max_turns }) => {
                assert_eq!(max_turns, 3);
            },
            other => panic!("expected UnsupportedMaxTurns, got: {other:?}"),
        }
    }

    #[test]
    fn auto_compact_failure_tracker_resets_on_success() {
        let tracker = AutoCompactFailureTracker::default();
        let session = SessionId::from("test-session");

        tracker.record_provider_failure(&session);
        tracker.record_provider_failure(&session);
        assert_eq!(tracker.consecutive_failures(&session), 2);

        tracker.record_provider_success(&session);
        assert_eq!(tracker.consecutive_failures(&session), 0);
    }
}
