//! LLM 驱动的上下文压缩模块。
//!
//! 当上下文窗口接近容量上限时，通过 LLM 对历史对话进行摘要压缩，
//! 保留关键信息的同时释放 token 空间。

/// 压缩配置参数。
///
/// 控制压缩行为的关键阈值和保留策略。
pub struct CompactConfig {
    /// 压缩时保留的最近对话轮数。
    pub keep_recent_turns: u8,
    /// 压缩时保留的最近用户消息条数。
    pub keep_recent_user_messages: u8,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub threshold_percent: u8,
    /// 压缩失败时的最大重试次数。
    pub max_retry_attempts: u8,
    /// LLM 压缩输出的最大 token 数。
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

/// 压缩操作的结果。
///
/// 记录压缩前后的 token 数量以及 LLM 生成的摘要文本。
pub struct CompactResult {
    /// 压缩前的 token 数量。
    pub pre_tokens: usize,
    /// 压缩后的 token 数量。
    pub post_tokens: usize,
    /// LLM 生成的对话摘要。
    pub summary: String,
}
