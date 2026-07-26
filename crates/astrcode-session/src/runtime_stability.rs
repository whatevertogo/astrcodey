use std::time::Duration;

use tokio::time::{Instant, sleep_until};

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
