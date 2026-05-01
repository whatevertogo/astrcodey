//! 上下文窗口运行时配置模块。
//!
//! 从运行时配置中派生的上下文窗口相关设置，
//! 控制自动压缩、文件追踪、恢复策略等行为。

/// 上下文窗口的完整配置项。
///
/// 涵盖自动压缩触发条件、文件追踪与恢复策略、摘要预留空间等参数。
#[derive(Debug, Clone)]
pub struct ContextWindowSettings {
    /// 是否启用自动压缩（当上下文占用达到阈值时自动触发）。
    pub auto_compact_enabled: bool,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub compact_threshold_percent: u8,
    /// 压缩时保留的最近对话轮数。
    pub compact_keep_recent_turns: u8,
    /// 压缩时额外按原文保留的最近真实用户消息条数。
    pub compact_keep_recent_user_messages: usize,
    /// 压缩失败时的最大重试次数。
    pub compact_max_retry_attempts: u8,
    /// 最大追踪文件数量（用于压缩后的文件恢复）。
    pub max_tracked_files: usize,
    /// 压缩后恢复上下文时最多重新加载的文件数。
    pub max_recovered_files: usize,
    /// 文件恢复的 token 预算上限。
    pub recovery_token_budget: usize,
    /// 为对话摘要预留的 token 数量。
    pub summary_reserve_tokens: usize,
    /// LLM 压缩输出的最大 token 数。
    pub compact_max_output_tokens: usize,
    /// 预留的上下文余量；低于该余量时即使百分比未达到也会触发压缩。
    pub reserved_context_tokens: usize,
    /// 单条工具结果的最大内联字节数。
    pub tool_result_max_bytes: usize,
    /// 最近一批工具结果的累计字节预算。
    pub aggregate_tool_result_bytes: usize,
    /// micro-compact 触发前允许保留的旧工具结果数量。
    pub micro_compact_keep_recent_results: usize,
    /// 工具结果空闲多久后可以进入 micro-compact，单位秒。
    pub micro_compact_gap_threshold_secs: u64,
}

impl Default for ContextWindowSettings {
    fn default() -> Self {
        Self {
            auto_compact_enabled: true,
            compact_threshold_percent: 85,
            compact_keep_recent_turns: 5,
            compact_keep_recent_user_messages: 8,
            compact_max_retry_attempts: 3,
            max_tracked_files: 64,
            max_recovered_files: 5,
            recovery_token_budget: 50000,
            summary_reserve_tokens: 20000,
            compact_max_output_tokens: 20000,
            reserved_context_tokens: 4096,
            tool_result_max_bytes: 8192,
            aggregate_tool_result_bytes: 24576,
            micro_compact_keep_recent_results: 5,
            micro_compact_gap_threshold_secs: 3600,
        }
    }
}
