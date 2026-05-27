//! ACP WebSocket 桥：IDE 客户端经 HTTP 升级连接，消息格式仍为 JSON-RPC。

use agent_client_protocol::{Channel, jsonrpcmsg};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};

use super::{AcpServices, run_agent};

/// 在单个 WebSocket 连接上服务 ACP Agent 端。
pub async fn serve_acp_websocket(
    socket: WebSocket,
    services: AcpServices,
) -> agent_client_protocol::Result<()> {
    let (client_channel, agent_channel) = Channel::duplex();
    let (mut ws_sink, mut ws_stream) = socket.split();

    let agent_task = tokio::spawn(async move { run_agent(services, agent_channel).await });

    let bridge_result = async {
        let Channel { mut rx, tx } = client_channel;
        loop {
            tokio::select! {
                ws_msg = ws_stream.next() => {
                    match ws_msg {
                        Some(Ok(Message::Text(text))) => {
                            let message: jsonrpcmsg::Message = serde_json::from_str(&text)
                                .map_err(|error| {
                                    agent_client_protocol::Error::parse_error()
                                        .data(format!("invalid JSON-RPC message: {error}"))
                                })?;
                            tx.unbounded_send(Ok(message)).map_err(|_| {
                                agent_client_protocol::Error::internal_error()
                                    .data("ACP client channel closed")
                            })?;
                        }
                        Some(Ok(Message::Binary(_))) => {
                            return Err(agent_client_protocol::Error::invalid_request()
                                .data("ACP websocket expects UTF-8 text JSON-RPC frames"));
                        }
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => {
                            return Err(agent_client_protocol::Error::internal_error()
                                .data(format!("websocket receive error: {error}")));
                        }
                    }
                }
                acp_msg = rx.next() => {
                    match acp_msg {
                        Some(Ok(message)) => {
                            let text = serde_json::to_string(&message).map_err(|error| {
                                agent_client_protocol::Error::internal_error()
                                    .data(format!("serialize JSON-RPC message: {error}"))
                            })?;
                            ws_sink
                                .send(Message::Text(text.into()))
                                .await
                                .map_err(|error| {
                                    agent_client_protocol::Error::internal_error()
                                        .data(format!("websocket send error: {error}"))
                                })?;
                        }
                        Some(Err(error)) => return Err(error),
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    agent_task.abort();
    let _ = agent_task.await;
    bridge_result
}
