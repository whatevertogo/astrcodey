use std::time::Duration;

use tokio::time::{Instant, sleep_until};

use crate::session_error::SessionError;

const DEFAULT_STABILITY_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_RETRY_DELAY: Duration = Duration::from_millis(5);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(100);

pub(crate) struct RuntimeStabilityBudget {
    deadline: Instant,
    retries: usize,
}

impl RuntimeStabilityBudget {
    pub(crate) fn new() -> Self {
        Self {
            deadline: Instant::now() + DEFAULT_STABILITY_TIMEOUT,
            retries: 0,
        }
    }

    /// 在运行时变更（如工具目录 revision 变化）后调用：给运行时留出稳定窗口。
    ///
    /// 每次调用递增重试计数并按指数退避睡眠，直到 [`DEFAULT_STABILITY_TIMEOUT`]
    /// 截止；窗口耗尽后返回 `Err(usize)`——错误值携带的是"已进行的重试次数"
    /// （不是错误码），调用方用它构造 [`crate::SessionError::RuntimeUnstable`]。
    /// 名字里的 retry 指"是否还能继续重试"：`Ok` = 窗口内，可重试；`Err` = 超时。
    pub(crate) async fn retry_after_change(&mut self) -> Result<(), usize> {
        self.retries += 1;

        let now = Instant::now();
        if now >= self.deadline {
            return Err(self.retries);
        }

        let shift = self.retries.saturating_sub(1).min(5);
        let delay = MIN_RETRY_DELAY
            .saturating_mul(1_u32 << shift)
            .min(MAX_RETRY_DELAY);
        sleep_until((now + delay).min(self.deadline)).await;

        if Instant::now() >= self.deadline {
            return Err(self.retries);
        }
        Ok(())
    }
}

pub(crate) async fn retry_runtime_snapshot(
    stability: &mut RuntimeStabilityBudget,
) -> Result<(), SessionError> {
    stability
        .retry_after_change()
        .await
        .map_err(|attempts| SessionError::RuntimeUnstable { attempts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_budget_reports_retry_count() {
        let mut budget = RuntimeStabilityBudget::new();
        budget.deadline = Instant::now();

        assert_eq!(budget.retry_after_change().await, Err(1));
    }
}
