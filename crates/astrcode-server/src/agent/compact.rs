//! Compact pipeline — hook 桥接 + forked provider 调用。
//!
//! 包含：
//! - [`CompactHookContext`]：在 compact 前后触发扩展钩子所需的上下文
//! - [`compact_with_forked_provider`]：使用独立 LLM 调用执行 compact
//!
//! 设计约束：这个模块不持有任何会话状态，所有参数通过调用方传入。

use std::sync::Arc;

use astrcode_context::{
    compaction::{
        CompactError, CompactResult, CompactSummaryRenderOptions, compact_messages_with_request,
    },
    settings::ContextWindowSettings,
};
use astrcode_core::{
    config::ModelSelection,
    extension::{
        CompactTrigger, ExtensionError, ExtensionEvent, PostCompactInput, PreCompactInput,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider},
    tool::ToolDefinition,
};
use astrcode_extensions::{context::ServerExtensionContext, runner::ExtensionRunner};

// ─── Hook 上下文 ─────────────────────────────────────────────────────────

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
        ModelSelection {
            profile_name: String::new(),
            model: input.model_id.to_string(),
            provider_kind: String::new(),
        },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForkQuerySource {
    Compact,
    #[allow(dead_code)]
    InternalAgent,
}

pub(crate) struct ForkedProviderRequest {
    pub(crate) base_messages: Vec<LlmMessage>,
    pub(crate) prompt_messages: Vec<LlmMessage>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) query_source: ForkQuerySource,
    pub(crate) max_turns: usize,
}

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

        let _query_source = request.query_source;
        let mut messages = request.base_messages;
        messages.extend(request.prompt_messages);
        let mut rx = self.llm.generate(messages, request.tools).await?;
        let mut text = String::new();

        while let Some(event) = rx.recv().await {
            match event {
                LlmEvent::ContentDelta { delta } => text.push_str(&delta),
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
                query_source: ForkQuerySource::Compact,
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
    settings: &ContextWindowSettings,
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
