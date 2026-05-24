use std::time::{Duration, Instant};

// 断路器状态：正常闭合、打开冷却中、半开准备重试
#[derive(Debug, Clone)]
enum CircuitState {
    // 正常状态，允许继续尝试调用 LLM
    Closed,
    // 断路器打开状态，在 until 之前拒绝调用
    Open { until: Instant },
    // 半开状态，表示冷却期已经结束，允许一次试探性调用
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CompactCircuitBreaker {
    // 当前断路器状态
    state: CircuitState,
    // 连续失败次数，仅在 Closed 或 HalfOpen 时累计
    consecutive_llm_failures: u32,
    // 触发打开断路器的失败阈值，最小为 1
    threshold: u32,
    // 断路器打开后的冷却期限
    cooldown: Duration,
    // 半开状态下是否已经有一次试探调用在飞行中
    half_open_attempt_in_flight: bool,
}

impl CompactCircuitBreaker {
    /// 创建一个新的紧凑断路器。
    ///
    /// `threshold` 为失败次数阈值，`cooldown` 为打开后的冷却时长。
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_llm_failures: 0,
            threshold: threshold.max(1),
            cooldown,
            half_open_attempt_in_flight: false,
        }
    }

    /// 重新配置断路器参数。
    pub fn reconfigure(&mut self, threshold: u32, cooldown: Duration) {
        self.threshold = threshold.max(1);
        self.cooldown = cooldown;
    }

    /// 判断当前是否允许继续尝试调用 LLM。
    ///
    /// Closed 状态总是允许。
    /// Open 状态在冷却期结束后切换到 HalfOpen，并允许一次试探调用。
    /// HalfOpen 状态只允许一次试探调用，之后必须等待结果。
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

    /// 记录一次 LLM 调用失败。
    ///
    /// 增加连续失败计数，并根据阈值决定是否打开断路器。
    pub fn record_llm_failure(&mut self) {
        self.consecutive_llm_failures = self.consecutive_llm_failures.saturating_add(1);
        self.trip_open_if_needed();
    }

    /// 记录一次成功调用。
    ///
    /// 成功后清除累计失败次数，并将断路器恢复到闭合状态。
    pub fn record_success(&mut self) {
        self.consecutive_llm_failures = 0;
        self.state = CircuitState::Closed;
        self.half_open_attempt_in_flight = false;
    }

    /// 记录一次自动压缩成功。
    ///
    /// 压缩成功后进入冷却期（Open 状态），防止短时间内重复压缩。
    /// 这是避免"压缩后立即又触发压缩"的关键机制。
    pub fn record_compact_success(&mut self) {
        self.consecutive_llm_failures = 0;
        self.state = CircuitState::Open {
            until: Instant::now() + self.cooldown,
        };
        self.half_open_attempt_in_flight = false;
    }

    /// 半开状态下是否允许新的试探调用。
    ///
    /// 如果已经有一次试探调用在进行中，则拒绝后续请求。
    fn allow_half_open_attempt(&mut self) -> bool {
        if self.half_open_attempt_in_flight {
            return false;
        }
        self.half_open_attempt_in_flight = true;
        true
    }

    /// 根据当前失败次数和状态决定是否打开断路器。
    ///
    /// HalfOpen 状态下出现失败也会立即重新打开断路器。
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

    #[test]
    fn record_compact_success_enters_cooldown() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(50));

        // 初始状态允许压缩
        assert!(breaker.should_attempt());

        // 压缩成功后进入冷却期
        breaker.record_compact_success();
        assert!(!breaker.should_attempt());

        // 冷却期内仍然不允许
        thread::sleep(Duration::from_millis(20));
        assert!(!breaker.should_attempt());

        // 冷却期结束后允许（HalfOpen 状态）
        thread::sleep(Duration::from_millis(40));
        assert!(breaker.should_attempt());

        // HalfOpen 状态只允许一次
        assert!(!breaker.should_attempt());

        // 成功后恢复为 Closed
        breaker.record_success();
        assert!(breaker.should_attempt());
    }

    #[test]
    fn record_compact_success_resets_failure_count() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(50));

        // 累积失败次数
        breaker.record_llm_failure();
        breaker.record_llm_failure();
        assert!(!breaker.should_attempt()); // 断路器已打开

        // 压缩成功后重置失败计数并进入冷却期
        breaker.record_compact_success();

        // 等待冷却期结束
        thread::sleep(Duration::from_millis(60));

        // 现在应该允许尝试（HalfOpen 状态）
        assert!(breaker.should_attempt());
        // HalfOpen 状态只允许一次尝试
        assert!(!breaker.should_attempt());

        // 成功后恢复为 Closed
        breaker.record_success();
        assert!(breaker.should_attempt());

        // 再次失败不会立即打开断路器（因为计数已重置）
        breaker.record_llm_failure();
        assert!(breaker.should_attempt());
        breaker.record_llm_failure();
        assert!(!breaker.should_attempt()); // 现在才打开
    }
}
