use astrcode_core::{
    llm::{LlmMessage, LlmProvider, ModelLimits},
    prompt::{PromptProvider, SystemPromptInput},
};

use crate::{
    compaction::{
        CompactError, CompactPromptStyle, CompactRequestOptions, CompactResult, CompactSkipReason,
        CompactSummaryRenderOptions, CompactTextRunner, compact_messages_with_provider,
        compact_messages_with_render_options, compact_messages_with_runner_options,
    },
    prompt::composer::PromptComposer,
    settings::ContextWindowSettings,
    token_usage::{build_prompt_snapshot, should_compact},
};

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
}

/// 已准备好的 provider 消息。
///
/// system prompt 不在这里返回；server 可以继续用自己的 system-message 前缀，
/// 这里负责返回 compact 后的可见消息窗口。
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<CompactResult>,
}

/// 高层入口输入：同时包含 system prompt 组装和上下文窗口管理所需数据。
pub struct LlmContextInput<'a> {
    pub system_prompt_input: SystemPromptInput,
    pub history: Vec<LlmMessage>,
    pub user_message: Option<LlmMessage>,
    pub model_limits: ModelLimits,
    pub provider: Option<&'a dyn LlmProvider>,
}

/// 高层入口输出：server 可直接拿去构造 provider request。
#[derive(Debug, Clone)]
pub struct PreparedLlmContext {
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<CompactResult>,
}

/// LLM 上下文组装门面。
///
/// 它把 prompt composer、token gate 与 compact pipeline 串起来；
/// 不持有 provider/model limits，避免模型切换后沿用旧窗口。
pub struct LlmContextAssembler {
    settings: ContextWindowSettings,
    prompt: PromptComposer,
}

pub type ContextManager = LlmContextAssembler;

impl LlmContextAssembler {
    /// 创建上下文组装器；settings 是稳定策略，模型窗口由每次 request 输入提供。
    pub fn new(settings: ContextWindowSettings) -> Self {
        Self {
            settings,
            prompt: PromptComposer::new(),
        }
    }

    pub async fn prepare(
        &self,
        input: LlmContextInput<'_>,
    ) -> Result<PreparedLlmContext, CompactError> {
        let system_prompt = self.assemble_system_prompt(input.system_prompt_input).await;
        let mut messages = input.history;
        if let Some(user_message) = input.user_message {
            messages.push(user_message);
        }

        let prepared = if let Some(provider) = input.provider {
            self.prepare_provider_messages_with_provider(
                ContextPrepareInput {
                    messages,
                    system_prompt: Some(&system_prompt),
                    model_limits: input.model_limits,
                },
                provider,
            )
            .await?
        } else {
            self.prepare_provider_messages(ContextPrepareInput {
                messages,
                system_prompt: Some(&system_prompt),
                model_limits: input.model_limits,
            })
        };

        Ok(PreparedLlmContext {
            system_prompt,
            messages: prepared.messages,
            compaction: prepared.compaction,
        })
    }

    pub async fn assemble_system_prompt(&self, input: SystemPromptInput) -> String {
        self.prompt
            .assemble(input)
            .await
            .system_prompt
            .unwrap_or_default()
    }

    /// 准备 provider 可见消息；达到阈值时使用 deterministic compact fallback。
    ///
    /// 这个入口不需要 LLM/provider，适合测试或没有 provider-backed compact 的路径。
    pub fn prepare_provider_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext {
        let mut messages = input.messages;
        let snapshot = self.snapshot(&messages, input.system_prompt, input.model_limits);
        let compaction = if self.settings.auto_compact_enabled && should_compact(snapshot) {
            match self.compact_provider_messages(messages.clone(), input.system_prompt) {
                Ok((compacted_messages, compaction)) => {
                    messages = compacted_messages;
                    Some(compaction)
                },
                Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => None,
            }
        } else {
            None
        };

        PreparedContext {
            messages,
            compaction,
        }
    }

    /// 只判断当前输入是否会触发 automatic compact。
    ///
    /// server 用它在真正 compact 前收集 PreCompact hook 指令，避免低于阈值时
    /// 无意义地执行 hook。
    pub fn should_compact_provider_messages(&self, input: &ContextPrepareInput<'_>) -> bool {
        self.settings.auto_compact_enabled && {
            should_compact(self.snapshot(
                &input.messages,
                input.system_prompt,
                input.model_limits.clone(),
            ))
        }
    }

    /// 准备 provider 可见消息；达到阈值时优先使用 provider-backed compact。
    pub async fn prepare_provider_messages_with_provider(
        &self,
        input: ContextPrepareInput<'_>,
        provider: &dyn LlmProvider,
    ) -> Result<PreparedContext, CompactError> {
        let mut messages = input.messages;
        let snapshot = self.snapshot(&messages, input.system_prompt, input.model_limits);
        let compaction = if self.settings.auto_compact_enabled && should_compact(snapshot) {
            let prepared = match self
                .compact_provider_messages_with_provider(
                    provider,
                    messages.clone(),
                    input.system_prompt,
                )
                .await
            {
                Ok(prepared) => prepared,
                Err(_) => {
                    let (fallback_messages, fallback_compaction) =
                        self.compact_provider_messages(messages.clone(), input.system_prompt)?;
                    PreparedContext {
                        messages: fallback_messages,
                        compaction: Some(fallback_compaction),
                    }
                },
            };
            messages = prepared.messages;
            prepared.compaction
        } else {
            None
        };

        Ok(PreparedContext {
            messages,
            compaction,
        })
    }

    /// 准备 provider 可见消息；达到阈值时通过调用方提供的 compact runner 生成摘要。
    ///
    /// runner 把“如何调用 LLM”留给 server/runtime；context 只负责 compact 语义。
    pub async fn prepare_provider_messages_with_compact_runner(
        &self,
        input: ContextPrepareInput<'_>,
        prompt_style: CompactPromptStyle,
        runner: &dyn CompactTextRunner,
    ) -> Result<PreparedContext, CompactError> {
        self.prepare_provider_messages_with_compact_runner_options(
            input,
            CompactRequestOptions::new(prompt_style),
            runner,
        )
        .await
    }

    /// 与 runner-based prepare 相同，但允许调用方追加 compact prompt 指令。
    pub async fn prepare_provider_messages_with_compact_runner_options(
        &self,
        input: ContextPrepareInput<'_>,
        options: CompactRequestOptions,
        runner: &dyn CompactTextRunner,
    ) -> Result<PreparedContext, CompactError> {
        let mut messages = input.messages;
        let snapshot = self.snapshot(&messages, input.system_prompt, input.model_limits);
        let compaction = if self.settings.auto_compact_enabled && should_compact(snapshot) {
            let render_options = options.render_options.clone();
            let prepared = match self
                .compact_provider_messages_with_compact_runner_options(
                    runner,
                    messages.clone(),
                    input.system_prompt,
                    options,
                )
                .await
            {
                Ok(prepared) => prepared,
                Err(_) => {
                    let (fallback_messages, fallback_compaction) = self
                        .compact_provider_messages_with_render_options(
                            messages.clone(),
                            input.system_prompt,
                            render_options,
                        )?;
                    PreparedContext {
                        messages: fallback_messages,
                        compaction: Some(fallback_compaction),
                    }
                },
            };
            messages = prepared.messages;
            prepared.compaction
        } else {
            None
        };

        Ok(PreparedContext {
            messages,
            compaction,
        })
    }

    /// 对已有 provider messages 执行 deterministic compact。
    pub fn compact_provider_messages(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
    ) -> Result<(Vec<LlmMessage>, CompactResult), CompactSkipReason> {
        self.compact_provider_messages_with_render_options(
            messages,
            system_prompt,
            CompactSummaryRenderOptions::default(),
        )
    }

    /// 对已有 provider messages 执行 deterministic compact，并使用指定渲染选项。
    pub fn compact_provider_messages_with_render_options(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        render_options: CompactSummaryRenderOptions,
    ) -> Result<(Vec<LlmMessage>, CompactResult), CompactSkipReason> {
        let compaction =
            compact_messages_with_render_options(&messages, system_prompt, &render_options)?;
        let compacted_messages = [
            compaction.context_messages.clone(),
            compaction.retained_messages.clone(),
        ]
        .concat();
        Ok((compacted_messages, compaction))
    }

    /// 对已有 provider messages 执行 provider-backed compact。
    pub async fn compact_provider_messages_with_provider(
        &self,
        provider: &dyn LlmProvider,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
    ) -> Result<PreparedContext, CompactError> {
        let compaction =
            compact_messages_with_provider(provider, &messages, system_prompt, &self.settings)
                .await?;
        let compacted_messages = [
            compaction.context_messages.clone(),
            compaction.retained_messages.clone(),
        ]
        .concat();
        Ok(PreparedContext {
            messages: compacted_messages,
            compaction: Some(compaction),
        })
    }

    /// 对已有 provider messages 执行 runner-backed compact。
    pub async fn compact_provider_messages_with_compact_runner(
        &self,
        runner: &dyn CompactTextRunner,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        prompt_style: CompactPromptStyle,
    ) -> Result<PreparedContext, CompactError> {
        self.compact_provider_messages_with_compact_runner_options(
            runner,
            messages,
            system_prompt,
            CompactRequestOptions::new(prompt_style),
        )
        .await
    }

    /// 与 runner-backed compact 相同，但允许调用方控制 prompt style 与附加指令。
    pub async fn compact_provider_messages_with_compact_runner_options(
        &self,
        runner: &dyn CompactTextRunner,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        options: CompactRequestOptions,
    ) -> Result<PreparedContext, CompactError> {
        let compaction = compact_messages_with_runner_options(
            runner,
            &messages,
            system_prompt,
            &self.settings,
            options,
        )
        .await?;
        let compacted_messages = [
            compaction.context_messages.clone(),
            compaction.retained_messages.clone(),
        ]
        .concat();
        Ok(PreparedContext {
            messages: compacted_messages,
            compaction: Some(compaction),
        })
    }

    fn snapshot(
        &self,
        messages: &[LlmMessage],
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
    ) -> crate::token_usage::PromptTokenSnapshot {
        build_prompt_snapshot(
            messages,
            system_prompt,
            model_limits,
            self.settings.compact_threshold_percent,
        )
    }
}


#[cfg(test)]
mod tests {
    use astrcode_core::llm::LlmRole;

    use super::*;

    #[test]
    fn prepare_provider_messages_uses_current_model_limits_each_call() {
        let assembler = LlmContextAssembler::new(ContextWindowSettings::default());
        let messages = vec![
            LlmMessage::user("old user ".repeat(400)),
            LlmMessage::assistant("old answer ".repeat(400)),
            LlmMessage::user("current"),
        ];

        let large_window = assembler.prepare_provider_messages(ContextPrepareInput {
            messages: messages.clone(),
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens: 200_000,
                max_output_tokens: 1024,
            },
        });
        let small_window = assembler.prepare_provider_messages(ContextPrepareInput {
            messages,
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens: 100,
                max_output_tokens: 1024,
            },
        });

        assert!(large_window.compaction.is_none());
        assert!(small_window.compaction.is_some());
        assert!(small_window.messages.first().is_some_and(|message| {
            message.role == LlmRole::User
                && message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("<compact_summary>")))
        }));
    }
}
