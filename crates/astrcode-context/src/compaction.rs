//! LLM-driven context compaction.

pub struct CompactConfig {
    pub keep_recent_turns: u8,
    pub keep_recent_user_messages: u8,
    pub threshold_percent: u8,
    pub max_retry_attempts: u8,
    pub max_output_tokens: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            keep_recent_turns: 5,
            keep_recent_user_messages: 3,
            threshold_percent: 90,
            max_retry_attempts: 3,
            max_output_tokens: 200000,
        }
    }
}

pub struct CompactResult {
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
}
