use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CompactCircuitBreaker {
    state: CircuitState,
    consecutive_llm_failures: u32,
    threshold: u32,
    cooldown: Duration,
    half_open_attempt_in_flight: bool,
}

impl CompactCircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_llm_failures: 0,
            threshold: threshold.max(1),
            cooldown,
            half_open_attempt_in_flight: false,
        }
    }

    pub fn reconfigure(&mut self, threshold: u32, cooldown: Duration) {
        self.threshold = threshold.max(1);
        self.cooldown = cooldown;
    }

    pub fn should_attempt(&mut self) -> bool {
        match &self.state {
            CircuitState::Closed => true,
            CircuitState::Open { until } => {
                if Instant::now() >= *until {
                    self.state = CircuitState::HalfOpen;
                    self.half_open_attempt_in_flight = false;
                    self.allow_half_open_attempt()
                } else {
                    false
                }
            },
            CircuitState::HalfOpen => self.allow_half_open_attempt(),
        }
    }

    pub fn record_llm_failure(&mut self) {
        self.consecutive_llm_failures = self.consecutive_llm_failures.saturating_add(1);
        self.trip_open_if_needed();
    }

    pub fn record_success(&mut self) {
        self.consecutive_llm_failures = 0;
        self.state = CircuitState::Closed;
        self.half_open_attempt_in_flight = false;
    }

    fn allow_half_open_attempt(&mut self) -> bool {
        if self.half_open_attempt_in_flight {
            return false;
        }
        self.half_open_attempt_in_flight = true;
        true
    }

    fn trip_open_if_needed(&mut self) {
        if matches!(self.state, CircuitState::HalfOpen)
            || self.consecutive_llm_failures >= self.threshold
        {
            self.state = CircuitState::Open {
                until: Instant::now() + self.cooldown,
            };
            self.half_open_attempt_in_flight = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::CompactCircuitBreaker;

    #[test]
    fn breaker_opens_after_threshold_then_recovers_via_half_open() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(5));

        assert!(breaker.should_attempt());
        breaker.record_llm_failure();
        assert!(breaker.should_attempt());
        breaker.record_llm_failure();
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.record_success();
        assert!(breaker.should_attempt());
    }
}
