//! Frame rate limiter — clamps draw notifications to a maximum of 120 FPS.
//!
//! Ported from codex-rs `tui/src/tui/frame_rate_limiter.rs`.

use std::time::{Duration, Instant};

/// 120 FPS minimum frame interval (≈8.33ms).
pub(super) const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_334);

#[derive(Debug, Default)]
pub(super) struct FrameRateLimiter {
    last_emitted_at: Option<Instant>,
}

impl FrameRateLimiter {
    /// Returns `requested`, clamped forward if it would exceed the maximum frame rate.
    pub(super) fn clamp_deadline(&self, requested: Instant) -> Instant {
        let Some(last) = self.last_emitted_at else {
            return requested;
        };
        let min_allowed = last.checked_add(MIN_FRAME_INTERVAL).unwrap_or(last);
        requested.max(min_allowed)
    }

    pub(super) fn mark_emitted(&mut self, at: Instant) {
        self.last_emitted_at = Some(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_does_not_clamp() {
        let t0 = Instant::now();
        let limiter = FrameRateLimiter::default();
        assert_eq!(limiter.clamp_deadline(t0), t0);
    }

    #[test]
    fn clamps_to_min_interval_since_last_emit() {
        let t0 = Instant::now();
        let mut limiter = FrameRateLimiter::default();
        limiter.mark_emitted(t0);
        let too_soon = t0 + Duration::from_millis(1);
        assert_eq!(limiter.clamp_deadline(too_soon), t0 + MIN_FRAME_INTERVAL);
    }
}
