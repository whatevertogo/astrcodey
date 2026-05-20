//! Frame scheduling: FrameRequester actor + FrameRateLimiter.
//!
//! Design ported from codex-rs `tui/src/tui/frame_requester.rs`.
//! Any component or task can clone a `FrameRequester` and call
//! `schedule_frame()` to request a redraw. The internal `FrameScheduler`
//! task coalesces requests and enforces the 120 FPS cap before notifying
//! the main event loop via a broadcast channel.

pub mod event_stream;
mod rate_limiter;

use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use self::rate_limiter::FrameRateLimiter;

/// Lightweight handle for requesting future frame draws.
///
/// Clone freely — all clones share the same scheduler task.
#[derive(Clone, Debug)]
pub struct FrameRequester {
    tx: mpsc::UnboundedSender<Instant>,
}

impl FrameRequester {
    /// Create a new `FrameRequester` and spawn its associated scheduler task.
    ///
    /// `draw_tx` is the broadcast channel the main event loop listens on.
    pub fn new(draw_tx: broadcast::Sender<()>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(FrameScheduler::new(rx, draw_tx).run());
        Self { tx }
    }

    /// Request a frame draw as soon as possible.
    pub fn schedule_frame(&self) {
        let _ = self.tx.send(Instant::now());
    }

    /// Request a frame draw after `dur`.
    pub fn schedule_frame_in(&self, dur: Duration) {
        let _ = self.tx.send(Instant::now() + dur);
    }
}

// ─── Internal scheduler ───────────────────────────────────────────────────────

struct FrameScheduler {
    rx: mpsc::UnboundedReceiver<Instant>,
    draw_tx: broadcast::Sender<()>,
    limiter: FrameRateLimiter,
}

impl FrameScheduler {
    fn new(rx: mpsc::UnboundedReceiver<Instant>, draw_tx: broadcast::Sender<()>) -> Self {
        Self {
            rx,
            draw_tx,
            limiter: FrameRateLimiter::default(),
        }
    }

    async fn run(mut self) {
        loop {
            // Wait for the next request.
            let Some(requested) = self.rx.recv().await else {
                break; // sender dropped → main loop exited
            };
            let deadline = self.limiter.clamp_deadline(requested);
            let now = Instant::now();
            if deadline > now {
                tokio::time::sleep(deadline - now).await;
            }
            // Drain any additional requests that arrived while sleeping.
            while self.rx.try_recv().is_ok() {}
            let emit_at = Instant::now();
            self.limiter.mark_emitted(emit_at);
            let _ = self.draw_tx.send(());
        }
    }
}
