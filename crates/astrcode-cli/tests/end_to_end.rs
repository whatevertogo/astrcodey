//! 端到端集成测试：CLI → InProcessTransport → server runtime → 响应。
//!
//! 使用 [`test_transport`] 的轻量 runtime（Mock LLM），避免完整 bootstrap 卡住。

use std::time::Duration;

use astrcode_client::client::AstrcodeClient;
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

mod test_transport;

use test_transport::InProcessTransport;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

async fn in_process_client() -> (
    AstrcodeClient<InProcessTransport>,
    astrcode_client::stream::ConversationStream,
) {
    let client = AstrcodeClient::new(InProcessTransport::start());
    let stream = client.subscribe_events().await.unwrap();
    (client, stream)
}

async fn recv_event(
    stream: &mut astrcode_client::stream::ConversationStream,
) -> Result<ClientNotification, String> {
    tokio::time::timeout(EVENT_TIMEOUT, stream.recv())
        .await
        .map_err(|_| {
            format!(
                "timed out after {}s waiting for server event",
                EVENT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|_| "event stream closed".into())
}

/// 端到端：创建会话并提交 prompt，验证 TurnStarted → TurnCompleted。
#[tokio::test]
async fn test_e2e_create_session_and_prompt() {
    let (client, mut stream) = in_process_client().await;

    client
        .send_command(&ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap();

    let session_id = match recv_event(&mut stream).await.unwrap() {
        ClientNotification::Event(event)
            if matches!(event.payload, EventPayload::SessionStarted { .. }) =>
        {
            event.session_id
        },
        other => panic!("Expected SessionStarted event, got {:?}", other),
    };
    assert!(!session_id.as_str().is_empty());

    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: "Hello, astrcode!".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut got_turn_start = false;
    let mut got_message = false;

    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let notification = match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Ok(notification)) => notification,
            Ok(Err(_)) => break,
            Err(_) => {
                panic!(
                    "timed out after {}s waiting for turn lifecycle (got_turn_start={got_turn_start}, got_message={got_message})",
                    EVENT_TIMEOUT.as_secs()
                );
            },
        };

        let ClientNotification::Event(event) = notification else {
            continue;
        };
        match event.payload {
            EventPayload::TurnStarted => got_turn_start = true,
            EventPayload::AssistantTextDelta { .. } => got_message = true,
            EventPayload::TurnCompleted { .. } => {
                assert!(got_turn_start, "Should have received TurnStarted");
                assert!(
                    got_message,
                    "Mock LLM should produce AssistantTextDelta through in-process path"
                );
                return;
            },
            _ => {},
        }
    }

    panic!(
        "TurnCompleted not received within {}s (got_turn_start={got_turn_start}, got_message={got_message})",
        EVENT_TIMEOUT.as_secs()
    );
}

/// 端到端：ListSessions 返回 SessionList 通知。
#[tokio::test]
async fn test_e2e_list_sessions() {
    let (client, mut stream) = in_process_client().await;

    client
        .send_command(&ClientCommand::ListSessions)
        .await
        .unwrap();

    match recv_event(&mut stream).await.unwrap() {
        ClientNotification::SessionList { sessions } => {
            assert!(sessions.is_empty() || !sessions[0].session_id.is_empty());
        },
        other => panic!("Expected SessionList, got: {:?}", other),
    }
}
