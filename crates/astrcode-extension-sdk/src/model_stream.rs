use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

pub use astrcode_extension_contract::protocol::ModelStreamEvent;
use astrcode_extension_contract::{TerminalStream, WireErrorCode, protocol::ErrorPayload};
use futures_util::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MODEL_STREAM_BUFFER_CAPACITY: usize = 32;
pub const MODEL_STREAM_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Incremental model output. A terminal event is yielded once, followed by `None` forever.
///
/// Terminal and premature-close semantics are shared with the wire-side `PeerStream`
/// via [`TerminalStream`]; this wrapper adds process-local cancellation on drop.
pub struct ModelStream {
    stream: TerminalStream,
    cancellation: CancellationToken,
}

impl Stream for ModelStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(context)
    }
}

impl ModelStream {
    #[doc(hidden)]
    pub fn from_stream(
        stream: impl Stream<Item = ModelStreamEvent> + Send + 'static,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            stream: TerminalStream::new(Box::pin(stream), Arc::new(Mutex::new(None))),
            cancellation,
        }
    }
}

impl Drop for ModelStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[doc(hidden)]
pub struct ModelStreamSender {
    sender: mpsc::Sender<ModelStreamEvent>,
    cancellation: CancellationToken,
    close_error: Arc<Mutex<Option<ErrorPayload>>>,
    terminal_sent: bool,
}

impl ModelStreamSender {
    pub async fn send(&mut self, event: ModelStreamEvent) -> Result<(), ModelStreamSendError> {
        if self.terminal_sent {
            return Err(ModelStreamSendError::Terminated);
        }
        let terminal = event.is_terminal();
        match tokio::time::timeout(MODEL_STREAM_BACKPRESSURE_TIMEOUT, self.sender.send(event)).await
        {
            Ok(Ok(())) => {
                self.terminal_sent = terminal;
                Ok(())
            },
            Ok(Err(_)) => Err(ModelStreamSendError::ReceiverClosed),
            Err(_) => {
                self.cancellation.cancel();
                *self
                    .close_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ErrorPayload::new(
                    WireErrorCode::BackpressureTimeout,
                    "model stream consumer did not release capacity before the timeout",
                ));
                Err(ModelStreamSendError::BackpressureTimeout)
            },
        }
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelStreamSendError {
    #[error("model stream already emitted a terminal event")]
    Terminated,
    #[error("model stream receiver was closed")]
    ReceiverClosed,
    #[error("model stream backpressure timeout")]
    BackpressureTimeout,
}

#[doc(hidden)]
pub fn model_stream_channel(cancellation: CancellationToken) -> (ModelStreamSender, ModelStream) {
    let (sender, receiver) = mpsc::channel(MODEL_STREAM_BUFFER_CAPACITY);
    let close_error = Arc::new(Mutex::new(None));
    (
        ModelStreamSender {
            sender,
            cancellation: cancellation.clone(),
            close_error: Arc::clone(&close_error),
            terminal_sent: false,
        },
        ModelStream {
            stream: TerminalStream::new(Box::pin(ReceiverStream { receiver }), close_error),
            cancellation,
        },
    )
}

struct ReceiverStream {
    receiver: mpsc::Receiver<ModelStreamEvent>,
}

impl Stream for ReceiverStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[tokio::test]
    async fn stream_enforces_one_terminal_fuses_and_cancels_on_drop() {
        let cancellation = CancellationToken::new();
        let (mut sender, mut stream) = model_stream_channel(cancellation.clone());
        sender
            .send(ModelStreamEvent::ContentDelta {
                content: "hello".into(),
            })
            .await
            .unwrap();
        sender
            .send(ModelStreamEvent::Completed {
                output: serde_json::json!({ "content": "hello" }),
            })
            .await
            .unwrap();
        assert_eq!(
            sender.send(ModelStreamEvent::Started).await.unwrap_err(),
            ModelStreamSendError::Terminated
        );

        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::ContentDelta { content }) if content == "hello"
        ));
        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::Completed { .. })
        ));
        assert!(stream.next().await.is_none());
        assert!(stream.next().await.is_none());
        drop(stream);
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn sender_close_synthesizes_exactly_one_failed_terminal() {
        let (sender, mut stream) = model_stream_channel(CancellationToken::new());
        drop(sender);
        let Some(ModelStreamEvent::Failed { error }) = stream.next().await else {
            panic!("closed producer must synthesize a failed terminal event");
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::StreamClosed));
        assert!(stream.next().await.is_none());
    }
}
