use std::time::{Duration, Instant};

use astrcode_context::compaction::LlmCompactAttempt;
use parking_lot::Mutex;

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

pub(crate) struct CompactAttemptPermit<'a> {
    breaker: &'a Mutex<CompactCircuitBreaker>,
    finished: bool,
}

impl<'a> CompactAttemptPermit<'a> {
    pub(crate) fn acquire(breaker: &'a Mutex<CompactCircuitBreaker>) -> Option<Self> {
        let allowed = breaker.lock().should_attempt();
        allowed.then_some(Self {
            breaker,
            finished: false,
        })
    }

    pub(crate) fn finish(mut self, outcome: LlmCompactAttempt) {
        self.breaker.lock().finish_attempt(outcome);
        self.finished = true;
    }
}

impl Drop for CompactAttemptPermit<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker
                .lock()
                .finish_attempt(LlmCompactAttempt::NotAttempted);
        }
    }
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

    /// Applies the latest runtime settings without discarding session-local failure history.
    pub(crate) fn configure(&mut self, threshold: u32, cooldown: Duration) {
        self.threshold = threshold.max(1);
        self.cooldown = cooldown;
        if matches!(self.state, CircuitState::Closed)
            && self.consecutive_llm_failures >= self.threshold
        {
            self.start_cooldown();
        }
    }

    fn should_attempt(&mut self) -> bool {
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

    fn finish_attempt(&mut self, outcome: LlmCompactAttempt) {
        match outcome {
            LlmCompactAttempt::Failed => {
                self.consecutive_llm_failures = self.consecutive_llm_failures.saturating_add(1);
                if matches!(self.state, CircuitState::HalfOpen)
                    || self.consecutive_llm_failures >= self.threshold
                {
                    self.start_cooldown();
                }
            },
            LlmCompactAttempt::Succeeded => {
                self.consecutive_llm_failures = 0;
                self.state = CircuitState::Closed;
                self.half_open_attempt_in_flight = false;
            },
            LlmCompactAttempt::NotAttempted => {
                if matches!(self.state, CircuitState::HalfOpen) {
                    self.start_cooldown();
                }
            },
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

    use astrcode_context::compaction::LlmCompactAttempt;
    use parking_lot::Mutex;

    use super::{CircuitState, CompactAttemptPermit, CompactCircuitBreaker};

    #[test]
    fn breaker_enforces_threshold_cooldown_and_single_half_open_probe() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(5));

        assert!(breaker.should_attempt());
        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(breaker.should_attempt());
        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.finish_attempt(LlmCompactAttempt::Succeeded);
        assert!(breaker.should_attempt());
    }

    #[test]
    fn failed_half_open_probe_must_be_finished_before_another_probe() {
        let mut breaker = CompactCircuitBreaker::new(1, Duration::from_millis(5));

        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());
    }

    #[test]
    fn skipped_probe_does_not_reset_failures_or_close_half_open_state() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(5));

        assert!(breaker.should_attempt());
        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(breaker.should_attempt());
        breaker.finish_attempt(LlmCompactAttempt::NotAttempted);
        assert!(
            breaker.should_attempt(),
            "closed state must preserve the first failure"
        );
        breaker.finish_attempt(LlmCompactAttempt::Failed);
        assert!(
            !breaker.should_attempt(),
            "preserved failure must still open the breaker"
        );

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        breaker.finish_attempt(LlmCompactAttempt::NotAttempted);
        assert!(
            !breaker.should_attempt(),
            "a skipped half-open probe must return to cooldown"
        );
    }

    #[test]
    fn dropped_half_open_permit_returns_to_cooldown_and_allows_a_later_probe() {
        let breaker = Mutex::new(CompactCircuitBreaker::new(1, Duration::from_secs(60)));
        CompactAttemptPermit::acquire(&breaker)
            .unwrap()
            .finish(LlmCompactAttempt::Failed);
        assert!(CompactAttemptPermit::acquire(&breaker).is_none());

        let expire_cooldown = || {
            let mut breaker = breaker.lock();
            let CircuitState::Open { until } = &mut breaker.state else {
                panic!("breaker must be cooling down");
            };
            *until = std::time::Instant::now();
        };

        expire_cooldown();
        let abandoned_probe = CompactAttemptPermit::acquire(&breaker).unwrap();
        assert!(CompactAttemptPermit::acquire(&breaker).is_none());
        drop(abandoned_probe);
        assert!(CompactAttemptPermit::acquire(&breaker).is_none());

        expire_cooldown();
        CompactAttemptPermit::acquire(&breaker)
            .unwrap()
            .finish(LlmCompactAttempt::Succeeded);
        assert!(CompactAttemptPermit::acquire(&breaker).is_some());
    }
}
