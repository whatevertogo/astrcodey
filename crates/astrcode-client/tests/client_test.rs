//! astrcode-client 集成测试。
//!
//! 覆盖传输层接口、ConversationStream、错误类型和 RPC 客户端核心逻辑。

use astrcode_client::{
    client::AstrcodeClient,
    error::ClientError,
    stream::{ConversationStream, StreamError},
    transport::{ClientTransport, TransportError},
};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::mpsc;

/// 空操作传输层，所有 subscribe 返回立即断开的 receiver。
struct DisconnectTransport;

#[async_trait::async_trait]
impl ClientTransport for DisconnectTransport {
    async fn send(
        &self,
        _command: &astrcode_protocol::commands::ClientCommand,
    ) -> Result<(), TransportError> {
        Ok(())
    }

    async fn subscribe(&self) -> Result<mpsc::Receiver<ClientNotification>, TransportError> {
        let (_, rx) = mpsc::channel::<ClientNotification>(1);
        Ok(rx)
    }
}

#[tokio::test]
async fn conversation_stream_returns_disconnected_on_drop() {
    let (_, rx) = mpsc::channel::<ClientNotification>(1);
    let mut stream = ConversationStream::new(rx);
    let err = stream.recv().await.unwrap_err();
    assert!(matches!(err, StreamError::Disconnected));
}

#[tokio::test]
async fn conversation_stream_drain_pending_returns_buffered() {
    let (tx, rx) = mpsc::channel::<ClientNotification>(1);
    let mut stream = ConversationStream::new(rx);
    tx.send(ClientNotification::ExtensionRegistryChanged)
        .await
        .unwrap();
    drop(tx);
    let items = stream.drain_pending();
    assert_eq!(items.len(), 1);
    // After drain, stream should return Disconnected
    let err = stream.recv().await.unwrap_err();
    assert!(matches!(err, StreamError::Disconnected));
}

#[tokio::test]
async fn client_error_display_includes_server_message() {
    let err = ClientError::Server("something went wrong".into());
    assert!(err.to_string().contains("something went wrong"));
}

#[tokio::test]
async fn client_with_disconnected_transport_fails_wait() {
    let client = AstrcodeClient::new(DisconnectTransport);
    let result = client.list_sessions().await;
    assert!(result.is_err());
}
