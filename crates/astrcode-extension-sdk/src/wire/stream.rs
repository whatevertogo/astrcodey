//! 模型事件流的 terminal 语义原语。
//!
//! `TerminalStream` 是 `ModelStream`（sdk，进程内）与 `PeerStream`（wire）共享的
//! 包装：terminal 事件恰好产出一次，之后恒为 `None`；生产端在 terminal 之前
//! 关闭时，从 `close_error` 槽取错误合成一次 `Failed`（槽为空则合成
//! `StreamClosed`）。两边各自叠加传输特有的取消与观测逻辑。

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::Stream;

use crate::wire::{
    WireErrorCode,
    protocol::{ErrorPayload, ModelStreamEvent},
};

pub struct TerminalStream {
    inner: Pin<Box<dyn Stream<Item = ModelStreamEvent> + Send>>,
    close_error: Arc<Mutex<Option<ErrorPayload>>>,
    terminated: bool,
}

impl TerminalStream {
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = ModelStreamEvent> + Send>>,
        close_error: Arc<Mutex<Option<ErrorPayload>>>,
    ) -> Self {
        Self {
            inner,
            close_error,
            terminated: false,
        }
    }
}

impl Stream for TerminalStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                if event.is_terminal() {
                    self.terminated = true;
                }
                Poll::Ready(Some(event))
            },
            Poll::Ready(None) => {
                self.terminated = true;
                let error = self
                    .close_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                    .unwrap_or_else(|| {
                        ErrorPayload::new(
                            WireErrorCode::StreamClosed,
                            "model stream closed before a terminal event",
                        )
                    });
                Poll::Ready(Some(ModelStreamEvent::Failed { error }))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, stream};

    use super::*;

    #[tokio::test]
    async fn terminal_stream_emits_exactly_one_terminal_event() {
        let close_error = Arc::new(Mutex::new(Some(ErrorPayload::new(
            WireErrorCode::Transport,
            "peer closed",
        ))));
        let mut failed = TerminalStream::new(Box::pin(stream::empty()), close_error);
        assert!(matches!(
            failed.next().await,
            Some(ModelStreamEvent::Failed { error })
                if error.code_enum() == Some(WireErrorCode::Transport)
        ));
        assert!(failed.next().await.is_none());

        let mut completed_then_extra = TerminalStream::new(
            Box::pin(stream::iter([
                ModelStreamEvent::Completed {
                    output: serde_json::json!({ "content": "done" }),
                },
                ModelStreamEvent::ContentDelta {
                    content: "ignored".into(),
                },
            ])),
            Arc::new(Mutex::new(None)),
        );
        assert!(matches!(
            completed_then_extra.next().await,
            Some(ModelStreamEvent::Completed { output }) if output["content"] == "done"
        ));
        assert!(completed_then_extra.next().await.is_none());

        let mut closed = TerminalStream::new(Box::pin(stream::empty()), Arc::new(Mutex::new(None)));
        assert!(matches!(
            closed.next().await,
            Some(ModelStreamEvent::Failed { error })
                if error.code_enum() == Some(WireErrorCode::StreamClosed)
        ));
        assert!(closed.next().await.is_none());
    }
}
