//! Typed RPC client for astrcode server communication.

use std::sync::Arc;

use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::*, events::*};

use crate::{
    error::ClientError,
    stream::ConversationStream,
    transport::{ClientTransport, TransportError},
};

/// Typed client for the astrcode JSON-RPC server.
pub struct AstrcodeClient<T: ClientTransport> {
    transport: Arc<T>,
}

impl<T: ClientTransport> AstrcodeClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    async fn send(&self, cmd: &ClientCommand) -> Result<ClientNotification, ClientError> {
        Ok(self.transport.execute(cmd).await?)
    }

    /// Create a new session.
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

    /// Submit a prompt to the active session.
    pub async fn submit_prompt(&self, text: &str) -> Result<(), ClientError> {
        let cmd = ClientCommand::SubmitPrompt {
            text: text.into(),
            attachments: vec![],
        };
        self.send(&cmd).await?;
        Ok(())
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionListItem>, ClientError> {
        let cmd = ClientCommand::ListSessions;
        match self.send(&cmd).await? {
            ClientNotification::SessionList { sessions } => Ok(sessions),
            ClientNotification::Error { message, .. } => Err(ClientError::Server(message)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Fork a session.
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

    /// Subscribe to the server's event stream BEFORE sending commands.
    /// This ensures no events are missed.
    pub async fn subscribe_events(&self) -> Result<ConversationStream, ClientError> {
        let rx = self.transport.subscribe().await?;
        Ok(ConversationStream::new(rx))
    }

    /// Send a command without waiting for response (use with subscribe_events).
    pub async fn send_command(&self, cmd: &ClientCommand) -> Result<(), ClientError> {
        self.transport.send(cmd).await?;
        Ok(())
    }

    /// Send a command and get the raw server event response.
    pub async fn send_raw(&self, cmd: &ClientCommand) -> Result<ClientNotification, ClientError> {
        self.send(cmd).await
    }

    /// Abort the current turn.
    pub async fn abort(&self) -> Result<(), ClientError> {
        let cmd = ClientCommand::Abort;
        self.send(&cmd).await?;
        Ok(())
    }
}

/// Mock transport for testing.
pub struct MockTransport;

#[async_trait::async_trait]
impl ClientTransport for MockTransport {
    async fn send(&self, _command: &ClientCommand) -> Result<(), TransportError> {
        Ok(())
    }

    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ClientNotification>, TransportError> {
        let (_, rx) = tokio::sync::broadcast::channel(16);
        Ok(rx)
    }
}
