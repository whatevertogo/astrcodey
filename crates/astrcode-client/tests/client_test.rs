//! astrcode-client 集成测试。
//!
//! 覆盖传输层接口、ConversationStream、错误类型和 RPC 客户端核心逻辑。

use astrcode_client::{
    client::AstrcodeClient,
    error::ClientError,
    transport::{ClientTransport, TransportError},
};
use astrcode_protocol::events::ClientNotification;

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

    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ClientNotification>, TransportError> {
        let (tx, rx) = tokio::sync::broadcast::channel::<ClientNotification>(1);
        drop(tx);
        Ok(rx)
    }
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
