use std::sync::Arc;

use astrcode_context::{
    compaction::{
        CompactError, CompactResult, CompactSummaryRenderOptions, compact_messages_with_request,
    },
    settings::ContextWindowSettings,
};
use astrcode_core::{
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider},
    tool::ToolDefinition,
};

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
