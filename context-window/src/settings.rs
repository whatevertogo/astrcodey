use std::time::Duration;

use astrcode_core::ResolvedRuntimeConfig;

use super::{file_access::FileRecoveryConfig, micro_compact::MicroCompactConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWindowSettings {
    pub auto_compact_enabled: bool,
    pub compact_threshold_percent: u8,
    pub reserved_context_size: usize,
    pub summary_reserve_tokens: usize,
    pub compact_max_output_tokens: usize,
    pub compact_max_retry_attempts: usize,
    pub tool_result_max_bytes: usize,
    pub compact_keep_recent_turns: usize,
    pub compact_keep_recent_user_messages: usize,
    pub max_tracked_files: usize,
    pub max_recovered_files: usize,
    pub recovery_token_budget: usize,
    pub aggregate_result_bytes_budget: usize,
    pub micro_compact_gap_threshold: Duration,
    pub micro_compact_keep_recent_results: usize,
}

impl ContextWindowSettings {
    pub fn micro_compact_config(&self) -> MicroCompactConfig {
        MicroCompactConfig {
            gap_threshold: self.micro_compact_gap_threshold,
            keep_recent_results: self.micro_compact_keep_recent_results,
        }
    }

    pub fn file_recovery_config(&self) -> FileRecoveryConfig {
        FileRecoveryConfig {
            max_tracked_files: self.max_tracked_files,
            max_recovered_files: self.max_recovered_files,
            recovery_token_budget: self.recovery_token_budget,
        }
    }
}

impl From<&ResolvedRuntimeConfig> for ContextWindowSettings {
    fn from(config: &ResolvedRuntimeConfig) -> Self {
        Self {
            auto_compact_enabled: config.auto_compact_enabled,
            compact_threshold_percent: config.compact_threshold_percent,
            reserved_context_size: config.reserved_context_size.max(1),
            summary_reserve_tokens: config.summary_reserve_tokens.max(1),
            compact_max_output_tokens: config.compact_max_output_tokens.max(1),
            compact_max_retry_attempts: usize::from(config.compact_max_retry_attempts.max(1)),
            tool_result_max_bytes: config.tool_result_max_bytes,
            compact_keep_recent_turns: usize::from(config.compact_keep_recent_turns),
            compact_keep_recent_user_messages: usize::from(
                config.compact_keep_recent_user_messages.max(1),
            ),
            max_tracked_files: config.max_tracked_files,
            max_recovered_files: config.max_recovered_files.max(1),
            recovery_token_budget: config.recovery_token_budget.max(1),
            aggregate_result_bytes_budget: config.aggregate_result_bytes_budget.max(1),
            micro_compact_gap_threshold: Duration::from_secs(
                config.micro_compact_gap_threshold_secs.max(1),
            ),
            micro_compact_keep_recent_results: config.micro_compact_keep_recent_results.max(1),
        }
    }
}
