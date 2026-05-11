//! ACP (Agent Client Protocol) server adapter.
//!
//! Bridges the ACP JSON-RPC protocol (over stdio) to astrcode's internal
//! CommandHandle / broadcast event architecture. This module is purely a
//! DTO-mapping boundary — no session-runtime types leak through.

mod events;

use std::{sync::Arc, time::Duration};

use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Dispatch, Error, Responder,
    schema::{
        AgentCapabilities, AgentNotification, CancelNotification, InitializeRequest,
        InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
        ProtocolVersion, SessionId as AcpSessionId, StopReason,
    },
};
use astrcode_core::{event::Event, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
    bootstrap::ServerRuntime,
    handler::{CommandHandle, HandlerError},
};

/// Run the ACP server, reading from stdin and writing to stdout.
///
/// This function blocks until the connection is closed or an unrecoverable
/// error occurs.
pub async fn run_acp_server(runtime: Arc<ServerRuntime>) -> agent_client_protocol::Result<()> {
    let (event_tx, _) = broadcast::channel(256);
    let command_handle = CommandHandle::spawn(runtime, event_tx.clone());

    Agent
        .builder()
        .name("astrcode")
        .on_receive_request(
            {
                async move |req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    let _ = req; // accept whatever version the client sends
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new())
                            .agent_info(agent_client_protocol::schema::Implementation::new(
                                "astrcode",
                                env!("CARGO_PKG_VERSION"),
                            )),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let command_handle = command_handle.clone();

                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let working_dir = req.cwd.to_string_lossy().to_string();
                    match command_handle.create_session(working_dir).await {
                        Ok(session_id) => {
                            let acp_sid = AcpSessionId::new(session_id.to_string());
                            responder.respond(NewSessionResponse::new(acp_sid))
                        },
                        Err(e) => responder.respond_with_internal_error(e.to_string()),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let command_handle = command_handle.clone();
                let event_tx = event_tx.clone();

                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    match handle_prompt(req, &command_handle, &event_tx, &cx).await {
                        Ok(stop_reason) => responder.respond(PromptResponse::new(stop_reason)),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let command_handle = command_handle.clone();

                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    let sid = SessionId::from(notif.session_id.to_string());
                    let _ = command_handle.abort_session(sid).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::schema::Error::method_not_found(),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}

async fn handle_prompt(
    req: PromptRequest,
    command_handle: &CommandHandle,
    event_tx: &broadcast::Sender<ClientNotification>,
    cx: &ConnectionTo<Client>,
) -> Result<StopReason, Error> {
    let session_id = SessionId::from(req.session_id.to_string());
    let text = prompt_to_text(&req.prompt)?;
    let mut event_rx = event_tx.subscribe();

    let turn_id = command_handle
        .submit_prompt_for_session(session_id.clone(), text)
        .await
        .map_err(handler_error_to_acp)?;

    loop {
        match event_rx.recv().await {
            Ok(ClientNotification::Event(event)) => {
                if !event_belongs_to_prompt(&event, &session_id, &turn_id) {
                    continue;
                }

                if let astrcode_core::event::EventPayload::TurnCompleted { finish_reason } =
                    &event.payload
                {
                    drain_events(&mut event_rx, &session_id, &turn_id, cx).await;
                    return Ok(stop_reason_from_finish_reason(finish_reason));
                }

                forward_event(&event, cx);
            },
            Ok(_) => {
                // Non-Event notifications (SessionResumed, etc.) are not part of ACP turns.
            },
            Err(broadcast::error::RecvError::Lagged(count)) => {
                tracing::warn!(count, "ACP event subscriber lagged");
            },
            Err(broadcast::error::RecvError::Closed) => {
                return Ok(StopReason::EndTurn);
            },
        }
    }
}

/// Drain remaining events from the broadcast channel for this session
/// and forward them as ACP notifications before the prompt response.
async fn drain_events(
    event_rx: &mut broadcast::Receiver<ClientNotification>,
    session_id: &SessionId,
    turn_id: &astrcode_core::types::TurnId,
    cx: &ConnectionTo<Client>,
) {
    loop {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Ok(ClientNotification::Event(event))) => {
                if event_belongs_to_prompt(&event, session_id, turn_id) {
                    forward_event(&event, cx);
                }
            },
            Ok(Ok(_)) => {},
            Ok(Err(broadcast::error::RecvError::Lagged(count))) => {
                tracing::warn!(count, "ACP event subscriber lagged while draining");
            },
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
        }
    }
}

fn forward_event(event: &Event, cx: &ConnectionTo<Client>) {
    if let Some(acp_notif) =
        events::to_session_notification(event.session_id.as_str(), &event.payload)
    {
        let agent_notif = AgentNotification::SessionNotification(acp_notif);
        let _ = cx.send_notification(agent_notif);
    }
}

fn event_belongs_to_prompt(
    event: &Event,
    session_id: &SessionId,
    turn_id: &astrcode_core::types::TurnId,
) -> bool {
    if event.session_id != *session_id {
        return false;
    }

    event
        .turn_id
        .as_ref()
        .is_none_or(|event_turn_id| event_turn_id == turn_id)
}

fn stop_reason_from_finish_reason(finish_reason: &str) -> StopReason {
    match finish_reason {
        "aborted" | "cancelled" | "interrupted" => StopReason::Cancelled,
        "length" | "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

fn prompt_to_text(blocks: &[agent_client_protocol::schema::ContentBlock]) -> Result<String, Error> {
    let mut parts = Vec::new();

    for block in blocks {
        match block {
            agent_client_protocol::schema::ContentBlock::Text(tc) => {
                if !tc.text.is_empty() {
                    parts.push(tc.text.clone());
                }
            },
            agent_client_protocol::schema::ContentBlock::ResourceLink(link) => {
                parts.push(resource_link_text(link));
            },
            agent_client_protocol::schema::ContentBlock::Image(_) => {
                return Err(unsupported_prompt_block("image"));
            },
            agent_client_protocol::schema::ContentBlock::Audio(_) => {
                return Err(unsupported_prompt_block("audio"));
            },
            agent_client_protocol::schema::ContentBlock::Resource(_) => {
                return Err(unsupported_prompt_block("embedded resource"));
            },
            _ => return Err(unsupported_prompt_block("unknown")),
        }
    }

    if parts.is_empty() {
        return Err(Error::invalid_params().data("prompt must contain text or resource links"));
    }

    Ok(parts.join("\n\n"))
}

fn resource_link_text(link: &agent_client_protocol::schema::ResourceLink) -> String {
    let label = link.title.as_deref().unwrap_or(&link.name);
    let mut text = format!("[Resource: {label}]\nURI: {}", link.uri);

    if let Some(description) = &link.description {
        text.push_str("\nDescription: ");
        text.push_str(description);
    }

    if let Some(mime_type) = &link.mime_type {
        text.push_str("\nMIME: ");
        text.push_str(mime_type);
    }

    text
}

fn unsupported_prompt_block(kind: &str) -> Error {
    Error::invalid_params().data(format!("ACP prompt {kind} blocks are not supported"))
}

fn handler_error_to_acp(error: HandlerError) -> Error {
    match error {
        HandlerError::TurnAlreadyRunning => Error::new(40900, error.to_string()),
        HandlerError::NoActiveSession => Error::new(40400, error.to_string()),
        HandlerError::SessionNotFound(_) => Error::new(40401, error.to_string()),
        HandlerError::UnknownCommand(_) => Error::invalid_params().data(error.to_string()),
        HandlerError::NoActiveTurn
        | HandlerError::CompactBlocked
        | HandlerError::CompactionSkipped(_)
        | HandlerError::Other(_) => Error::internal_error().data(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::{ContentBlock, ResourceLink, TextContent};
    use astrcode_core::{
        event::{Event, EventPayload},
        types::{SessionId, TurnId},
    };

    use super::*;

    #[test]
    fn prompt_to_text_keeps_text_and_resource_links() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("look at this")),
            ContentBlock::ResourceLink(
                ResourceLink::new("notes.md", "file:///workspace/notes.md")
                    .title(Some("Notes".to_string()))
                    .description(Some("project notes".to_string())),
            ),
        ];

        let text = prompt_to_text(&blocks).unwrap();

        assert!(text.contains("look at this"));
        assert!(text.contains("[Resource: Notes]"));
        assert!(text.contains("file:///workspace/notes.md"));
        assert!(text.contains("project notes"));
    }

    #[test]
    fn prompt_to_text_rejects_unsupported_blocks() {
        let err = prompt_to_text(&[ContentBlock::Resource(
            agent_client_protocol::schema::EmbeddedResource::new(
                agent_client_protocol::schema::EmbeddedResourceResource::TextResourceContents(
                    agent_client_protocol::schema::TextResourceContents::new(
                        "contents",
                        "file:///workspace/notes.md",
                    ),
                ),
            ),
        )])
        .unwrap_err();

        assert_eq!(err.code, Error::invalid_params().code);
    }

    #[test]
    fn event_filter_rejects_other_turn_completion_events() {
        let session_id = SessionId::from("session-1");
        let turn_id = TurnId::from("turn-1");
        let other_turn = TurnId::from("turn-2");
        let event = Event::new(
            session_id.clone(),
            Some(other_turn),
            EventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        );

        assert!(!event_belongs_to_prompt(&event, &session_id, &turn_id));
    }
}
