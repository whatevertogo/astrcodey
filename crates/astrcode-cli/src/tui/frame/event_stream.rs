//! TUI event stream: EventBroker, TuiEvent, EventStream.
//!
//! Ported from tui/tui_event/{mod,stream}.rs — merged into one file.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use crossterm::event::{Event, EventStream as CrosstermEventStream, KeyEvent, KeyEventKind};
use futures_util::Stream;
use parking_lot::Mutex;
use tokio_stream::wrappers::WatchStream;

// ─── EventBroker ─────────────────────────────────────────────────────────────

enum BrokerState {
    Start,
    Running(CrosstermEventStream),
}

/// Shared crossterm EventStream with pause/resume support.
pub struct EventBroker {
    state: Mutex<BrokerState>,
    resume_tx: tokio::sync::watch::Sender<()>,
}

impl EventBroker {
    pub fn new() -> Self {
        let (resume_tx, _) = tokio::sync::watch::channel(());
        Self {
            state: Mutex::new(BrokerState::Start),
            resume_tx,
        }
    }

    pub(crate) fn poll_crossterm_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Event>>> {
        let mut state = self.state.lock();
        if matches!(*state, BrokerState::Start) {
            *state = BrokerState::Running(CrosstermEventStream::new());
        }
        let BrokerState::Running(stream) = &mut *state else {
            return Poll::Pending;
        };
        Pin::new(stream)
            .poll_next(cx)
            .map(|r| r.map(|r| r.map_err(io::Error::other)))
    }

    pub(crate) fn resume_rx(&self) -> tokio::sync::watch::Receiver<()> {
        self.resume_tx.subscribe()
    }
}

// ─── TuiEvent ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Draw,
}

// ─── EventStream ─────────────────────────────────────────────────────────────

pub struct EventStream {
    broker: Option<EventBroker>,
    resume_stream: WatchStream<()>,
    _pin: std::marker::PhantomPinned,
}

impl Unpin for EventStream {}

impl EventStream {
    pub fn new(broker: EventBroker) -> Self {
        let resume_stream = WatchStream::from_changes(broker.resume_rx());
        Self {
            broker: Some(broker),
            resume_stream,
            _pin: std::marker::PhantomPinned,
        }
    }

    fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        loop {
            let broker = match &self.broker {
                Some(b) => b,
                None => return Poll::Ready(None),
            };
            match broker.poll_crossterm_event(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    if let Some(ev) = self.map_crossterm_event(event) {
                        return Poll::Ready(Some(ev));
                    }
                },
                Poll::Ready(Some(Err(e))) => {
                    tracing::error!("crossterm event stream error: {e}");
                    return Poll::Ready(None);
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn poll_resume_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.resume_stream).poll_next(cx) {
            Poll::Ready(Some(_)) => Poll::Ready(Some(TuiEvent::Draw)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn map_crossterm_event(&mut self, event: Event) -> Option<TuiEvent> {
        match event {
            Event::Key(key_event) => {
                if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    return None;
                }
                Some(TuiEvent::Key(key_event))
            },
            Event::Resize(_, _) => Some(TuiEvent::Draw),
            Event::Paste(text) => Some(TuiEvent::Paste(text)),
            Event::FocusGained => Some(TuiEvent::Draw),
            Event::FocusLost => None,
            _ => None,
        }
    }
}

impl Stream for EventStream {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(ev) = self.as_mut().poll_resume_event(cx) {
            return Poll::Ready(ev);
        }
        self.as_mut().poll_crossterm_event(cx)
    }
}
