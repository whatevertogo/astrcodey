//! 进程内传输层 —— 服务器在 tokio 任务中运行，无需子进程。
//!
//! 通过 mpsc 通道发送命令，通过 broadcast 通道接收事件，
//! 实现客户端与服务器的进程内通信。

use std::sync::Arc;

use astrcode_client::transport::{ClientTransport, TransportError};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_server::{bootstrap, handler::CommandHandler};
use tokio::sync::{broadcast, mpsc};

/// 进程内传输实现，在后台 tokio 任务中运行服务器。
///
/// 命令通过 `cmd_tx`（mpsc 通道）发送到服务器任务，
/// 事件通过 `event_tx`（broadcast 通道）广播给所有订阅者。
pub struct InProcessTransport {
    /// 命令发送端，将 ClientCommand 发送到服务器处理循环
    cmd_tx: mpsc::UnboundedSender<ClientCommand>,
    /// 事件广播发送端，服务器处理完命令后通过此通道广播通知
    event_tx: broadcast::Sender<ClientNotification>,
}

impl InProcessTransport {
    /// 启动后台服务器任务并返回已连接的传输实例。
    pub fn start() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCommand>();
        let (event_tx, _) = broadcast::channel::<ClientNotification>(256);
        let tx = event_tx.clone();

        // 在后台 tokio 任务中运行服务器
        tokio::spawn(async move {
            // 引导服务器运行时（加载配置、初始化组件等）
            let runtime = match bootstrap::bootstrap().await {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    // 引导失败时通过事件通道通知客户端
                    let _ = tx.send(ClientNotification::Error {
                        code: -32603,
                        message: e.to_string(),
                    });
                    return;
                },
            };

            // 创建命令 actor，循环接收并处理客户端命令
            let handler = CommandHandler::spawn_actor(runtime, tx);
            while let Some(cmd) = cmd_rx.recv().await {
                if let Err(e) = handler.handle(cmd).await {
                    // handler 内部已将错误事件广播给客户端，此处只做日志记录
                    tracing::error!("Command handler error: {e}");
                }
            }
        });

        Self { cmd_tx, event_tx }
    }
}

#[async_trait::async_trait]
impl ClientTransport for InProcessTransport {
    /// 通过 mpsc 通道发送命令到服务器任务。
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError> {
        self.cmd_tx
            .send(command.clone())
            .map_err(|_| TransportError::Connection("server task ended".into()))
    }

    /// 订阅事件广播通道，返回一个新的接收端。
    async fn subscribe(&self) -> Result<broadcast::Receiver<ClientNotification>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}
