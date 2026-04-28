//! Conversation stream wrapper for SSE events.

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

/// A subscription to the server event stream.
pub struct ConversationStream {
    rx: broadcast::Receiver<ClientNotification>,
}

impl ConversationStream {
    pub fn new(rx: broadcast::Receiver<ClientNotification>) -> Self {
        Self { rx }
    }

    /// Receive the next event from the stream.
    pub async fn recv(&mut self) -> Result<StreamItem, StreamError> {
        match self.rx.recv().await {
            Ok(event) => Ok(StreamItem::Event(event)),
            Err(broadcast::error::RecvError::Lagged(n)) => Ok(StreamItem::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => Err(StreamError::Disconnected),
        }
    }
}

/// Items received from the conversation stream.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum StreamItem {
    Event(ClientNotification),
    /// Client fell behind; `n` events were skipped.
    Lagged(u64),
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("Stream disconnected")]
    Disconnected,
}
