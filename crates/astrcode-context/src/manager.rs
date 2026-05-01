use astrcode_core::{
    llm::{LlmMessage, LlmProvider, ModelLimits},
    prompt::{PromptProvider, SystemPromptInput},
};

use crate::{
    compaction::{
        CompactError, CompactResult, CompactSkipReason, compact_messages,
        compact_messages_with_provider,
    },
    prompt::composer::PromptComposer,
    settings::ContextWindowSettings,
    token_usage::{build_prompt_snapshot, should_compact},
};

#[derive(Debug, Clone)]
pub struct ContextPrepareInput<'a> {
    pub messages: Vec<LlmMessage>,
    pub system_prompt: Option<&'a str>,
    pub model_limits: ModelLimits,
}

#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<CompactResult>,
}

pub struct LlmContextInput<'a> {
    pub system_prompt_input: SystemPromptInput,
    pub history: Vec<LlmMessage>,
    pub user_message: Option<LlmMessage>,
    pub model_limits: ModelLimits,
    pub provider: Option<&'a dyn LlmProvider>,
}

#[derive(Debug, Clone)]
pub struct PreparedLlmContext {
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<CompactResult>,
}

pub struct LlmContextAssembler {
    settings: ContextWindowSettings,
    prompt: PromptComposer,
}

pub type ContextManager = LlmContextAssembler;

impl LlmContextAssembler {
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

    pub fn compact_provider_messages(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
    ) -> Result<(Vec<LlmMessage>, CompactResult), CompactSkipReason> {
        let compaction = compact_messages(&messages, system_prompt)?;
        let compacted_messages = [
            compaction.context_messages.clone(),
            compaction.retained_messages.clone(),
        ]
        .concat();
        Ok((compacted_messages, compaction))
    }

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
