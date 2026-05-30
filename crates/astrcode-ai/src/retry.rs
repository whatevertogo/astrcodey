//! LLM API 调用的指数退避重试逻辑。
//!
//! 在遇到可重试的 HTTP 状态码（如 429 限流、5xx 服务端错误）时，
//! 按指数退避策略自动重试请求，并加入抖动以避免惊群效应。

use std::time::Duration;

/// 重试策略配置。
///
/// 控制最大重试次数和基础退避延迟。
pub struct RetryPolicy {
    /// 最大重试次数（HTTP 状态码触发）
    pub max_retries: u32,
    /// 基础退避延迟（毫秒），实际延迟为 base × 2^(attempt-1) ± 抖动
    pub base_delay_ms: u64,
    /// 传输层错误最大重试次数（连接重置、TLS 握手失败、DNS 临时错误等）。
    ///
    /// 这些错误不由 HTTP 状态码表示，是 reqwest 在无法取得响应时的底层错误。
    /// 独立于 `max_retries`，因为传输层错误通常是偶发性的（尤其 TLS "unexpected EOF"），
    /// 重试 1-2 次即可恢复，无需与 HTTP 级别重试共享计数。
    pub max_transport_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 250,
            max_transport_retries: 2,
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

    /// 判断传输层错误（TLS、连接重置、DNS 临时失败等）是否应该重试。
    ///
    /// 与 `should_retry` 使用独立的计数器，因为传输层错误通常是瞬态的，
    /// 短暂重试即可恢复，不应消耗 HTTP 级别的重试配额。
    pub fn should_retry_transport(&self, attempt: u32) -> bool {
        attempt <= self.max_transport_retries
    }

    /// 根据尝试次数计算指数退避延迟，并加入 ±25% 随机抖动。
    ///
    /// 加入抖动是为了在多个客户端同时重试时避免惊群效应。
    pub fn delay(&self, attempt: u32) -> Duration {
        let base = self.base_delay_ms * 2u64.pow(attempt.saturating_sub(1));
        let jitter = base / 4;
        let span = jitter.saturating_mul(2);
        let offset = random_jitter(span);
        Duration::from_millis(base.saturating_sub(jitter) + offset)
    }
}

fn random_jitter(span: u64) -> u64 {
    if span == 0 {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return span / 2;
    }
    u64::from_le_bytes(bytes) % (span + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_values() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.base_delay_ms, 250);
        assert_eq!(policy.max_transport_retries, 2);
    }

    #[test]
    fn should_retry_returns_true_for_retryable_status_codes() {
        let policy = RetryPolicy::default();
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                policy.should_retry(1, code),
                "expected {code} to be retryable"
            );
        }
    }

    #[test]
    fn should_retry_returns_false_for_non_retryable_status_codes() {
        let policy = RetryPolicy::default();
        for code in [400, 401, 403, 404, 405, 409, 422] {
            assert!(
                !policy.should_retry(1, code),
                "expected {code} to NOT be retryable"
            );
        }
    }

    #[test]
    fn should_retry_returns_false_after_max_retries_exceeded() {
        let policy = RetryPolicy::default();
        // max_retries=2, attempt=3 is beyond limit
        assert!(!policy.should_retry(3, 429));
        assert!(!policy.should_retry(99, 500));
        // attempt <= max_retries should still retry
        assert!(policy.should_retry(1, 429));
        assert!(policy.should_retry(2, 429));
    }

    #[test]
    fn should_retry_transport_within_limit() {
        let policy = RetryPolicy::default();
        assert!(policy.should_retry_transport(1));
        assert!(policy.should_retry_transport(2));
        assert!(!policy.should_retry_transport(3));
        assert!(!policy.should_retry_transport(99));
    }

    #[test]
    fn should_retry_transport_with_custom_limit() {
        let policy = RetryPolicy {
            max_transport_retries: 1,
            ..RetryPolicy::default()
        };
        assert!(policy.should_retry_transport(1));
        assert!(!policy.should_retry_transport(2));
        assert!(!policy.should_retry_transport(3));
    }

    #[test]
    fn delay_grows_exponentially() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay_ms: 100,
            max_transport_retries: 0,
        };
        let d1 = policy.delay(1).as_millis();
        let d2 = policy.delay(2).as_millis();
        let d3 = policy.delay(3).as_millis();

        // base: 100, 200, 400 (before jitter ±25%)
        // d2 should be roughly 2× d1, d3 roughly 4× d1
        assert!(d2 > d1, "delay should grow: d2({d2}) > d1({d1})");
        assert!(d3 > d2, "delay should grow: d3({d3}) > d2({d2})");
        // With jitter, d2 should be within [150, 250] (200 ± 25%)
        assert!(
            (150..=250).contains(&(d2 as u64)),
            "d2 ({d2}) should be roughly 200 ± 25%"
        );
    }
}
