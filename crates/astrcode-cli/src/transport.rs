//! 进程内传输层 —— 服务器在 tokio 任务中运行，无需子进程。
//!
//! 通过 mpsc 通道发送命令，通过 broadcast 通道接收事件，
//! 实现客户端与服务器的进程内通信。

use std::sync::Arc;

use astrcode_client::transport::{ClientTransport, TransportError};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_server::{bootstrap, events::ClientEventPublisher, router::CommandRouter};
use tokio::sync::{broadcast, mpsc, watch};

#[derive(Debug, Clone)]
enum BootstrapState {
    Starting,
    Ready,
    Failed(String),
}

/// 进程内传输实现，在后台 tokio 任务中运行服务器。
///
/// 命令通过 `cmd_tx`（mpsc 通道）发送到服务器任务，
/// 事件通过 `event_tx`（broadcast 通道）广播给所有订阅者。
pub struct InProcessTransport {
    /// 命令发送端，将 ClientCommand 发送到服务器处理循环
    cmd_tx: mpsc::UnboundedSender<ClientCommand>,
    /// 事件广播发送端，服务器处理完命令后通过此通道广播通知
    event_tx: broadcast::Sender<ClientNotification>,
    ready_rx: watch::Receiver<BootstrapState>,
}

impl InProcessTransport {
    /// 启动后台服务器任务并返回已连接的传输实例。
    pub fn start() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCommand>();
        let (event_tx, _) = broadcast::channel::<ClientNotification>(16384);
        let (ready_tx, ready_rx) = watch::channel(BootstrapState::Starting);
        let tx = event_tx.clone();

        // 在后台 tokio 任务中运行服务器
        tokio::spawn(async move {
            // 引导服务器运行时（加载配置、初始化组件等）
            let runtime = match bootstrap::bootstrap().await {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    let message = e.to_string();
                    let _ = ready_tx.send(BootstrapState::Failed(message.clone()));
                    // 引导失败时通过事件通道通知客户端
                    let _ = tx.send(ClientNotification::Error {
                        code: -32603,
                        message,
                    });
                    return;
                },
            };

            // 创建命令 actor，循环接收并处理客户端命令
            let event_publisher = Arc::new(ClientEventPublisher::new(tx));
            let handler = CommandRouter::spawn_actor(runtime, event_publisher);
            let _ = ready_tx.send(BootstrapState::Ready);
            while let Some(cmd) = cmd_rx.recv().await {
                if let Err(e) = handler.handle(cmd).await {
                    // handler 内部已将错误事件广播给客户端，此处只做日志记录
                    tracing::error!("Command handler error: {e}");
                }
            }
        });

        Self {
            cmd_tx,
            event_tx,
            ready_rx,
        }
    }

    async fn wait_until_ready(&self) -> Result<(), TransportError> {
        wait_for_bootstrap_ready(self.ready_rx.clone()).await
    }
}

async fn wait_for_bootstrap_ready(
    mut ready_rx: watch::Receiver<BootstrapState>,
) -> Result<(), TransportError> {
    loop {
        match &*ready_rx.borrow() {
            BootstrapState::Ready => return Ok(()),
            BootstrapState::Failed(message) => {
                return Err(TransportError::Connection(format!(
                    "server bootstrap failed: {message}"
                )));
            },
            BootstrapState::Starting => {},
        }

        if ready_rx.changed().await.is_err() {
            return Err(TransportError::Connection(
                "server task ended before bootstrap completed".into(),
            ));
        }
    }
}

#[async_trait::async_trait]
impl ClientTransport for InProcessTransport {
    /// 通过 mpsc 通道发送命令到服务器任务。
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError> {
        self.wait_until_ready().await?;
        self.cmd_tx
            .send(command.clone())
            .map_err(|_| TransportError::Connection("server task ended".into()))
    }

    /// 订阅事件广播通道，返回一个新的接收端。
    async fn subscribe(&self) -> Result<broadcast::Receiver<ClientNotification>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_bootstrap_ready_returns_bootstrap_failure() {
        let (ready_tx, ready_rx) = watch::channel(BootstrapState::Starting);
        ready_tx
            .send(BootstrapState::Failed("bad config".into()))
            .unwrap();

        let error = wait_for_bootstrap_ready(ready_rx).await.unwrap_err();

        assert!(error.to_string().contains("server bootstrap failed"));
        assert!(error.to_string().contains("bad config"));
    }

    #[tokio::test]
    async fn wait_for_bootstrap_ready_accepts_ready_state() {
        let (_ready_tx, ready_rx) = watch::channel(BootstrapState::Ready);

        wait_for_bootstrap_ready(ready_rx).await.unwrap();
    }
}
