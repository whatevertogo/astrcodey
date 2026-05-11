//! TUI 事件流管理模块。
//!
//! 提供统一的事件处理，支持键盘、粘贴、resize 和绘制事件。
//! 支持暂停/恢复事件流（用于外部程序）。

mod stream;

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use crossterm::event::{Event, EventStream as CrosstermEventStream};
use futures::Stream;
use parking_lot::Mutex;
pub use stream::{EventStream, TuiEvent};

/// 事件代理：共享的 crossterm EventStream。
///
/// 所有事件消费者通过这个代理访问同一个底层流，
/// 支持暂停/恢复以完全释放 stdin。
pub struct EventBroker {
    state: Mutex<BrokerState>,
    resume_tx: tokio::sync::watch::Sender<()>,
}

enum BrokerState {
    Start,
    Running(CrosstermEventStream),
}

impl EventBroker {
    /// 轮询 crossterm 事件。
    ///
    /// 如果已暂停，返回 Poll::Pending。
    pub(crate) fn poll_crossterm_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Event>>> {
        let mut state = self.state.lock();
        match &mut *state {
            BrokerState::Start => {
                *state = BrokerState::Running(CrosstermEventStream::new());
                if let BrokerState::Running(stream) = &mut *state {
                    Pin::new(stream)
                        .poll_next(cx)
                        .map(|r| r.map(|r| r.map_err(io::Error::other)))
                } else {
                    unreachable!()
                }
            },
            BrokerState::Running(stream) => Pin::new(stream)
                .poll_next(cx)
                .map(|r| r.map(|r| r.map_err(io::Error::other))),
        }
    }

    /// 创建新的事件代理。
    pub fn new() -> Self {
        let (resume_tx, _) = tokio::sync::watch::channel(());

        Self {
            state: Mutex::new(BrokerState::Start),
            resume_tx,
        }
    }

    /// 获取恢复通知的接收器。
    pub(crate) fn resume_rx(&self) -> tokio::sync::watch::Receiver<()> {
        self.resume_tx.subscribe()
    }
}

/// 终端焦点状态跟踪。
#[derive(Clone)]
pub struct TerminalFocus(Arc<AtomicBool>);

impl TerminalFocus {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub fn set_focused(&self, focused: bool) {
        self.0.store(focused, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn is_focused(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_broker_starts_in_start_state() {
        let broker = EventBroker::new();
        let state = broker.state.lock();
        assert!(matches!(*state, BrokerState::Start));
    }

    #[test]
    fn test_terminal_focus_default() {
        let focus = TerminalFocus::new();
        assert!(focus.is_focused());
    }

    #[test]
    fn test_terminal_focus_set() {
        let focus = TerminalFocus::new();
        focus.set_focused(false);
        assert!(!focus.is_focused());

        focus.set_focused(true);
        assert!(focus.is_focused());
    }
}
