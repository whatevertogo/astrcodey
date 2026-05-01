use std::path::Path;

use astrcode_core::{
    llm::{LlmMessage, ModelLimits},
    tool::ToolResult,
};

use crate::{
    budget::{
        ToolResultBudget, ToolResultBudgetStats, ToolResultReplacementState,
        apply_tool_result_budget,
    },
    compaction::{CompactResult, CompactSkipReason, compact_messages},
    file_access::{FileAccessTracker, FileRecoveryConfig},
    settings::ContextWindowSettings,
    token_usage::{PromptTokenSnapshot, TokenUsageTracker, build_prompt_snapshot, should_compact},
};

#[derive(Debug, Clone)]
pub struct ContextPrepareInput<'a> {
    pub messages: Vec<LlmMessage>,
    pub system_prompt: Option<&'a str>,
    pub model_limits: ModelLimits,
    pub persist_dir: Option<&'a Path>,
    pub working_dir: Option<&'a Path>,
}

#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub snapshot: PromptTokenSnapshot,
    pub budget_stats: ToolResultBudgetStats,
    pub compaction: Option<CompactResult>,
}

pub struct ContextManager {
    settings: ContextWindowSettings,
    budget: ToolResultBudget,
    token_usage: TokenUsageTracker,
    replacements: ToolResultReplacementState,
    file_access: FileAccessTracker,
}

impl ContextManager {
    pub fn new(settings: ContextWindowSettings) -> Self {
        let budget = ToolResultBudget::new(
            settings.tool_result_max_bytes,
            settings.tool_result_max_bytes / 2,
            settings.aggregate_tool_result_bytes,
        );
        let max_tracked_files = settings.max_tracked_files;
        Self {
            settings,
            budget,
            token_usage: TokenUsageTracker::new(),
            replacements: ToolResultReplacementState::default(),
            file_access: FileAccessTracker::new(max_tracked_files),
        }
    }

    pub fn record_tool_result(&mut self, tool_name: &str, result: &ToolResult) {
        self.file_access.record_tool_result(tool_name, result);
    }

    pub fn prepare_provider_messages(&mut self, input: ContextPrepareInput<'_>) -> PreparedContext {
        let budgeted = apply_tool_result_budget(
            &input.messages,
            &mut self.replacements,
            &self.budget,
            input.persist_dir,
        );
        let mut messages = budgeted.messages;
        let mut snapshot =
            self.snapshot(&messages, input.system_prompt, input.model_limits.clone());
        let compaction = if self.settings.auto_compact_enabled && should_compact(snapshot) {
            match self.compact_provider_messages(
                messages.clone(),
                input.system_prompt,
                input.model_limits,
                input.working_dir,
            ) {
                Ok(prepared) => {
                    messages = prepared.messages;
                    snapshot = prepared.snapshot;
                    prepared.compaction
                },
                Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => None,
            }
        } else {
            None
        };

        PreparedContext {
            messages,
            snapshot,
            budget_stats: budgeted.stats,
            compaction,
        }
    }

    pub fn compact_provider_messages(
        &mut self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
        working_dir: Option<&Path>,
    ) -> Result<PreparedContext, CompactSkipReason> {
        let mut compaction = compact_messages(&messages, system_prompt, &self.settings)?;
        if let Some(working_dir) = working_dir {
            compaction
                .context_messages
                .extend(self.file_access.build_recovery_messages(
                    working_dir,
                    FileRecoveryConfig {
                        max_recovered_files: self.settings.max_recovered_files,
                        recovery_token_budget: self.settings.recovery_token_budget,
                    },
                ));
        }
        let compacted_messages = [
            compaction.context_messages.clone(),
            compaction.retained_messages.clone(),
        ]
        .concat();
        let snapshot = self.snapshot(&compacted_messages, system_prompt, model_limits);
        Ok(PreparedContext {
            messages: compacted_messages,
            snapshot,
            budget_stats: ToolResultBudgetStats::default(),
            compaction: Some(compaction),
        })
    }

    fn snapshot(
        &self,
        messages: &[LlmMessage],
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
    ) -> PromptTokenSnapshot {
        build_prompt_snapshot(
            &self.token_usage,
            messages,
            system_prompt,
            model_limits,
            self.settings.compact_threshold_percent,
            self.settings.summary_reserve_tokens,
            self.settings.reserved_context_tokens,
        )
    }
}
