//! 类型化的 RPC 客户端，用于与 astrcode 服务端通信。
//!
//! 封装了 JSON-RPC 命令的发送与响应解析，提供会话管理、提示词提交、
//! 事件流订阅等高层 API。

use std::sync::Arc;

use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::*, events::*};

use crate::{
    error::ClientError,
    stream::ConversationStream,
    transport::{ClientTransport, TransportError},
};

/// 类型化的 astrcode JSON-RPC 客户端。
///
/// 通过泛型传输层 `T` 与服务端通信，支持 stdio 等多种传输方式。
pub struct AstrcodeClient<T: ClientTransport> {
    /// 底层传输层实例，使用 `Arc` 共享所有权以支持事件订阅。
    transport: Arc<T>,
}

impl<T: ClientTransport> AstrcodeClient<T> {
    /// 创建新的客户端实例。
    ///
    /// - `transport`: 底层传输层实现（如 `StdioClientTransport`）。
    pub fn new(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    /// 发送命令并等待服务端返回第一个响应通知。
    ///
    /// 内部调用传输层的 `execute` 方法，将传输错误映射为 `ClientError`。
    async fn send(&self, cmd: &ClientCommand) -> Result<ClientNotification, ClientError> {
        Ok(self.transport.execute(cmd).await?)
    }

    /// 创建新的会话。
    ///
    /// - `working_dir`: 会话的工作目录路径。
    /// - 返回新创建的会话 ID。
    pub async fn create_session(&self, working_dir: &str) -> Result<String, ClientError> {
        let cmd = ClientCommand::CreateSession {
            working_dir: working_dir.into(),
        };
        match self.send(&cmd).await? {
            ClientNotification::Event(event) => match event.payload {
                EventPayload::SessionStarted { .. } => Ok(event.session_id),
                _ => Err(ClientError::UnexpectedResponse),
            },
            ClientNotification::Error { message, .. } => Err(ClientError::Server(message)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// 向当前活跃会话提交提示词。
    ///
    /// - `text`: 用户输入的提示词文本。
    pub async fn submit_prompt(&self, text: &str) -> Result<(), ClientError> {
        let cmd = ClientCommand::SubmitPrompt {
            text: text.into(),
            attachments: vec![],
        };
        self.send(&cmd).await?;
        Ok(())
    }

    /// 列出所有会话。
    ///
    /// 返回会话列表，每项包含会话 ID 等摘要信息。
    pub async fn list_sessions(&self) -> Result<Vec<SessionListItem>, ClientError> {
        let cmd = ClientCommand::ListSessions;
        match self.send(&cmd).await? {
            ClientNotification::SessionList { sessions } => Ok(sessions),
            ClientNotification::Error { message, .. } => Err(ClientError::Server(message)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// 分叉（Fork）一个已有会话。
    ///
    /// - `session_id`: 要分叉的源会话 ID。
    /// - `at_cursor`: 可选的光标位置，指定从哪个历史节点分叉。
    /// - 返回新会话的 ID。
    pub async fn fork_session(
        &self,
        session_id: &str,
        at_cursor: Option<&str>,
    ) -> Result<String, ClientError> {
        let cmd = ClientCommand::ForkSession {
            session_id: session_id.into(),
            at_cursor: at_cursor.map(String::from),
        };
        match self.send(&cmd).await? {
            ClientNotification::Event(event) => match event.payload {
                EventPayload::SessionStarted { .. } => Ok(event.session_id),
                _ => Err(ClientError::UnexpectedResponse),
            },
            ClientNotification::Error { message, .. } => Err(ClientError::Server(message)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// 订阅服务端的事件流。
    ///
    /// **应在发送命令之前调用**，以确保不会遗漏任何事件。
    /// 返回一个 `ConversationStream`，可通过异步迭代接收事件。
    pub async fn subscribe_events(&self) -> Result<ConversationStream, ClientError> {
        let rx = self.transport.subscribe().await?;
        Ok(ConversationStream::new(rx))
    }

    /// 发送命令但不等待响应（需配合 `subscribe_events` 使用）。
    pub async fn send_command(&self, cmd: &ClientCommand) -> Result<(), ClientError> {
        self.transport.send(cmd).await?;
        Ok(())
    }

    /// 发送命令并获取原始服务端事件响应。
    pub async fn send_raw(&self, cmd: &ClientCommand) -> Result<ClientNotification, ClientError> {
        self.send(cmd).await
    }

    /// 中止当前轮次（abort）。
    pub async fn abort(&self) -> Result<(), ClientError> {
        let cmd = ClientCommand::Abort;
        self.send(&cmd).await?;
        Ok(())
    }
}

/// 用于测试的模拟传输层。
///
/// 所有操作均为空操作（no-op），适用于单元测试中不需要真实服务端的场景。
pub struct MockTransport;

#[async_trait::async_trait]
impl ClientTransport for MockTransport {
    async fn send(&self, _command: &ClientCommand) -> Result<(), TransportError> {
        Ok(())
    }

    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ClientNotification>, TransportError> {
        // 创建一个空的广播通道，返回接收端。
        let (_, rx) = tokio::sync::broadcast::channel(16);
        Ok(rx)
    }
}
