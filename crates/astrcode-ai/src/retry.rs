//! Exponential backoff retry logic for LLM API calls.

use std::time::Duration;

pub struct RetryPolicy {
    pub max_retries: u32,
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
    /// Check if a request should be retried based on status code and attempt count.
    pub fn should_retry(&self, attempt: u32, status: u16) -> bool {
        if attempt > self.max_retries {
            return false;
        }
        matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
    }

    /// Calculate the delay for a given attempt using exponential backoff.
    pub fn delay(&self, attempt: u32) -> Duration {
        Duration::from_millis(self.base_delay_ms * 2u64.pow(attempt - 1))
    }
}
