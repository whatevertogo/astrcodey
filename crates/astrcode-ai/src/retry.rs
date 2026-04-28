//! LLM API 调用的指数退避重试逻辑。
//!
//! 在遇到可重试的 HTTP 状态码（如 429 限流、5xx 服务端错误）时，
//! 按指数退避策略自动重试请求，并加入抖动以避免惊群效应。

use std::time::Duration;

/// 重试策略配置。
///
/// 控制最大重试次数和基础退避延迟。
pub struct RetryPolicy {
    /// 最大重试次数
    pub max_retries: u32,
    /// 基础退避延迟（毫秒），实际延迟为 base × 2^(attempt-1) ± 抖动
    pub base_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 250,
        }
    }
}

impl RetryPolicy {
    /// 根据状态码和尝试次数判断是否应该重试。
    ///
    /// 仅对以下状态码进行重试：408（超时）、429（限流）、500/502/503/504（服务端错误）。
    pub fn should_retry(&self, attempt: u32, status: u16) -> bool {
        if attempt > self.max_retries {
            return false;
        }
        matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
    }

    /// 根据尝试次数计算指数退避延迟，并加入 ±25% 抖动。
    ///
    /// 加入抖动是为了在多个客户端同时重试时避免惊群效应。
    pub fn delay(&self, attempt: u32) -> Duration {
        let base = self.base_delay_ms * 2u64.pow(attempt - 1);
        // 简单的确定性抖动：使用尝试次数作为伪随机种子
        let jitter = base / 4;
        let offset = (attempt as u64 * 17 + base % 31) % (jitter * 2 + 1);
        Duration::from_millis(base.saturating_sub(jitter) + offset)
    }
}
