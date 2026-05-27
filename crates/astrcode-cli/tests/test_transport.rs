//! 集成测试专用进程内传输：使用 [`astrcode_server::testing`] 轻量 runtime。

use std::sync::Arc;

use astrcode_client::transport::{ClientTransport, TransportError};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_server::{bootstrap, testing::in_process_test_runtime};
use astrcode_support::event_fanout::EventFanout;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, PartialEq, Eq)]
enum BootstrapState {
    Starting,
    Ready,
}

pub struct InProcessTransport {
    cmd_tx: mpsc::UnboundedSender<ClientCommand>,
    event_tx: Arc<EventFanout<ClientNotification>>,
    ready_rx: watch::Receiver<BootstrapState>,
}

impl InProcessTransport {
    pub fn start() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCommand>();
        let event_tx = Arc::new(EventFanout::new(1024));
        let (ready_tx, ready_rx) = watch::channel(BootstrapState::Starting);
        let tx = Arc::clone(&event_tx);

        tokio::spawn(async move {
            let runtime = in_process_test_runtime();
            let server_system = bootstrap::spawn_server_system(&runtime, tx);
            let handler = server_system.handler;
            let _ = ready_tx.send(BootstrapState::Ready);
            while let Some(cmd) = cmd_rx.recv().await {
                if let Err(error) = handler.handle(cmd).await {
                    tracing::error!("Command handler error: {error}");
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
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError> {
        self.wait_until_ready().await?;
        self.cmd_tx
            .send(command.clone())
            .map_err(|_| TransportError::Connection("server task ended".into()))
    }

    async fn subscribe(&self) -> Result<mpsc::Receiver<ClientNotification>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}
