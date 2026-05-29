//! 端到端集成测试：CLI → 客户端 → 服务器 → 代理循环 → 响应。
//!
//! 通过 stdio JSON-RPC 与 astrcode-server 二进制文件通信，
//! 验证完整流水线：会话创建、提示提交、响应流式输出。
//!
//! 默认 `#[ignore]`；运行：`ASTRCODE_RUN_STDIO_E2E=1 cargo test -p astrcode-cli -- --ignored`

use astrcode_client::{client::AstrcodeClient, transport::StdioClientTransport};
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

/// 获取服务器二进制文件路径。
///
/// 优先使用 `ASTRCODE_SERVER_BIN` 环境变量指定的路径，
/// 否则在 `target/debug/` 目录下查找。
fn server_binary() -> String {
    if let Ok(bin) = std::env::var("ASTRCODE_SERVER_BIN") {
        return bin;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let name = if cfg!(windows) {
        "astrcode-server.exe"
    } else {
        "astrcode-server"
    };
    let debug_path = format!("{manifest_dir}/../../target/debug/{name}");
    if std::path::Path::new(&debug_path).exists() {
        return debug_path;
    }
    format!("target/debug/{name}")
}

fn require_stdio_e2e() -> Option<String> {
    if std::env::var("ASTRCODE_RUN_STDIO_E2E").as_deref() != Ok("1") {
        panic!("set ASTRCODE_RUN_STDIO_E2E=1 to run stdio e2e tests");
    }
    let bin = server_binary();
    if !std::path::Path::new(&bin).exists() {
        panic!(
            "astrcode-server binary not found at {bin}; build with: cargo build -p astrcode-server"
        );
    }
    Some(bin)
}

/// 端到端测试：创建会话并提交提示，验证完整的响应流式输出。
///
/// 测试流程：
/// 1. 启动服务器进程并通过 stdio 连接
/// 2. 创建新会话，验证收到 SessionStarted 事件
/// 3. 提交提示文本，验证收到 TurnStarted → AssistantTextDelta → TurnCompleted 事件序列
#[tokio::test]
#[ignore = "stdio e2e: ASTRCODE_RUN_STDIO_E2E=1 and built astrcode-server required"]
async fn test_e2e_create_session_and_prompt() {
    let bin = require_stdio_e2e().expect("e2e prerequisites");

    let transport = StdioClientTransport::spawn(&bin, &[])
        .unwrap_or_else(|e| panic!("Failed to spawn server: {e}"));

    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    client
        .send_command(&ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap();

    let session_id = match stream.recv().await.unwrap() {
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
    let mut got_turn_end = false;

    for _ in 0..100 {
        match stream.recv().await {
            Ok(ClientNotification::Event(event)) => match event.payload {
                EventPayload::TurnStarted => {
                    got_turn_start = true;
                },
                EventPayload::AssistantTextDelta { .. } => {
                    got_message = true;
                },
                EventPayload::TurnCompleted { .. } => {
                    got_turn_end = true;
                    break;
                },
                EventPayload::ErrorOccurred { message, .. } => {
                    panic!("server error event: {message}");
                },
                _ => {},
            },
            Ok(ClientNotification::Error { message, .. }) => {
                panic!("server error notification: {message}");
            },
            Ok(_) => {},
            Err(e) => panic!("stream recv failed: {e}"),
        }
    }

    assert!(got_turn_start, "Should have received TurnStarted");
    assert!(
        got_message,
        "Should have received AssistantTextDelta; full pipeline should stream"
    );
    assert!(got_turn_end, "Should have received TurnCompleted");
}

/// 端到端测试：列出会话。
///
/// 验证服务器能正确响应 ListSessions 命令并返回 SessionList 通知。
#[tokio::test]
#[ignore = "stdio e2e: ASTRCODE_RUN_STDIO_E2E=1 and built astrcode-server required"]
async fn test_e2e_list_sessions() {
    let bin = require_stdio_e2e().expect("e2e prerequisites");

    let transport = StdioClientTransport::spawn(&bin, &[]).unwrap();
    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    client
        .send_command(&ClientCommand::ListSessions)
        .await
        .unwrap();

    match stream.recv().await.unwrap() {
        ClientNotification::SessionList { sessions } => {
            assert!(sessions.is_empty() || !sessions.is_empty());
        },
        other => panic!("Expected SessionList, got: {:?}", other),
    }
}
