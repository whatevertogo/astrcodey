use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::wire::TerminalStream;
pub use crate::wire::protocol::ModelStreamEvent;

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
