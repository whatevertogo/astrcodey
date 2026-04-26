//! Context window settings derived from runtime config.

pub struct ContextWindowSettings {
    pub auto_compact_enabled: bool,
    pub compact_threshold_percent: u8,
    pub compact_keep_recent_turns: u8,
    pub compact_max_retry_attempts: u8,
    pub max_tracked_files: usize,
    pub max_recovered_files: usize,
    pub recovery_token_budget: usize,
    pub summary_reserve_tokens: usize,
    pub compact_max_output_tokens: usize,
}

impl Default for ContextWindowSettings {
    fn default() -> Self {
        Self {
            auto_compact_enabled: true,
            compact_threshold_percent: 90,
            compact_keep_recent_turns: 5,
            compact_max_retry_attempts: 3,
            max_tracked_files: 64,
            max_recovered_files: 16,
            recovery_token_budget: 8192,
            summary_reserve_tokens: 2048,
            compact_max_output_tokens: 4096,
        }
    }
}
