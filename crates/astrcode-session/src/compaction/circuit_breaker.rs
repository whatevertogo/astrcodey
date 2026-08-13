use std::time::{Duration, Instant};

#[derive(Debug)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

/// Auto LLM compact 的进程内熔断状态。
#[derive(Debug)]
pub(crate) struct CompactCircuitBreaker {
    state: CircuitState,
    consecutive_llm_failures: u32,
    threshold: u32,
    cooldown: Duration,
    half_open_attempt_in_flight: bool,
}

impl CompactCircuitBreaker {
    pub(crate) fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_llm_failures: 0,
            threshold: threshold.max(1),
            cooldown,
            half_open_attempt_in_flight: false,
        }
    }

    pub(crate) fn should_attempt(&mut self) -> bool {
        match &self.state {
            CircuitState::Closed => true,
            CircuitState::Open { until } => {
                if Instant::now() < *until {
                    return false;
                }
                self.state = CircuitState::HalfOpen;
                self.half_open_attempt_in_flight = false;
                self.allow_half_open_attempt()
            },
            CircuitState::HalfOpen => self.allow_half_open_attempt(),
        }
    }

    /// `should_attempt` 放行后必须调用，无论后续在哪个阶段结束。
    pub(crate) fn finish_attempt(&mut self, llm_failed: bool) {
        if llm_failed {
            self.consecutive_llm_failures = self.consecutive_llm_failures.saturating_add(1);
            if matches!(self.state, CircuitState::HalfOpen)
                || self.consecutive_llm_failures >= self.threshold
            {
                self.start_cooldown();
            }
        } else {
            self.consecutive_llm_failures = 0;
            self.start_cooldown();
        }
    }

    fn allow_half_open_attempt(&mut self) -> bool {
        if self.half_open_attempt_in_flight {
            return false;
        }
        self.half_open_attempt_in_flight = true;
        true
    }

    fn start_cooldown(&mut self) {
        self.state = CircuitState::Open {
            until: Instant::now() + self.cooldown,
        };
        self.half_open_attempt_in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::CompactCircuitBreaker;

    #[test]
    fn breaker_enforces_threshold_cooldown_and_single_half_open_probe() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(5));

        assert!(breaker.should_attempt());
        breaker.finish_attempt(true);
        assert!(breaker.should_attempt());
        breaker.finish_attempt(true);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.finish_attempt(false);
        assert!(!breaker.should_attempt());
        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());
    }

    #[test]
    fn failed_half_open_probe_must_be_finished_before_another_probe() {
        let mut breaker = CompactCircuitBreaker::new(1, Duration::from_millis(5));

        breaker.finish_attempt(true);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.finish_attempt(true);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());
    }
}
