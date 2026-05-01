//! 上下文窗口运行时配置模块。
//!
//! 从运行时配置中派生的上下文窗口相关设置，
//! 控制自动压缩和摘要压缩行为。

/// 上下文窗口的完整配置项。
///
/// 涵盖自动压缩触发条件与压缩请求参数。
#[derive(Debug, Clone)]
pub struct ContextWindowSettings {
    /// 是否启用自动压缩（当上下文占用达到阈值时自动触发）。
    pub auto_compact_enabled: bool,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub compact_threshold_percent: f32,
    /// 压缩失败时的最大重试次数。
    pub compact_max_retry_attempts: u8,
    /// LLM 压缩输出的最大 token 数。
    pub compact_max_output_tokens: usize,
}

impl Default for ContextWindowSettings {
    fn default() -> Self {
        Self {
            // TODO: 后期需要让用户可控制
            auto_compact_enabled: true,
            compact_threshold_percent: 83.5,
            compact_max_retry_attempts: 3,
            compact_max_output_tokens: 20000,
        }
    }
}
