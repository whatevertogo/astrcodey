//! 端到端集成测试：CLI → 客户端 → 服务器 → 代理循环 → 响应。
//!
//! 通过 stdio JSON-RPC 与 astrcode-server 二进制文件通信，
//! 验证完整流水线：会话创建、提示提交、响应流式输出。
//!
//! 默认跳过，需设置环境变量 `ASTRCODE_RUN_STDIO_E2E=1` 才会执行。

use astrcode_client::{client::AstrcodeClient, transport::StdioClientTransport};
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

/// 获取服务器二进制文件路径。
///
/// 优先使用 `ASTRCODE_SERVER_BIN` 环境变量指定的路径，
/// 否则在 `target/debug/` 目录下查找。
fn server_binary() -> String {
    std::env::var("ASTRCODE_SERVER_BIN").unwrap_or_else(|_| {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        let debug_path = format!("{}/../../target/debug/astrcode-server.exe", manifest_dir);
        if std::path::Path::new(&debug_path).exists() {
            return debug_path;
        }
        "target/debug/astrcode-server.exe".into()
    })
}

/// 检查端到端测试是否启用。
fn stdio_e2e_enabled() -> bool {
    std::env::var("ASTRCODE_RUN_STDIO_E2E").as_deref() == Ok("1")
}

/// 端到端测试：创建会话并提交提示，验证完整的响应流式输出。
///
/// 测试流程：
/// 1. 启动服务器进程并通过 stdio 连接
/// 2. 创建新会话，验证收到 SessionStarted 事件
/// 3. 提交提示文本，验证收到 TurnStarted → AssistantTextDelta → TurnCompleted 事件序列
#[tokio::test]
async fn test_e2e_create_session_and_prompt() {
    if !stdio_e2e_enabled() {
        eprintln!("Skipping stdio e2e; set ASTRCODE_RUN_STDIO_E2E=1 to run it");
        return;
    }

    let bin = server_binary();
    if !std::path::Path::new(&bin).exists() {
        eprintln!("Skipping e2e test: server binary not found at {}", bin);
        eprintln!("Build it first: cargo build -p astrcode-server");
        return;
    }

    // 通过 stdio 启动服务器进程
    let transport = match StdioClientTransport::spawn(&bin, &[]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to spawn server: {}", e);
            return;
        },
    };

    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    // 创建会话
    client
        .send_command(&ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap();

    // 验证收到 SessionStarted 事件
    let session_id = match stream.recv().await.unwrap() {
        astrcode_client::stream::StreamItem::Event(ClientNotification::Event(event))
            if matches!(event.payload, EventPayload::SessionStarted { .. }) =>
        {
            event.session_id
        },
        other => panic!("Expected SessionStarted event, got {:?}", other),
    };
    assert!(!session_id.is_empty());

    // 提交提示
    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: "Hello, astrcode!".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // 验证完整的事件序列：TurnStarted → AssistantTextDelta → TurnCompleted
    let mut got_turn_start = false;
    let mut got_message = false;
    let mut got_turn_end = false;

    for _ in 0..100 {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(notification)) => match notification {
                ClientNotification::Event(event) => match event.payload {
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
                        eprintln!("server error event: {message}");
                    },
                    _ => {},
                },
                ClientNotification::Error { message, .. } => {
                    eprintln!("server error notification: {message}");
                },
                _ => {},
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {},
            Err(_) => break,
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
async fn test_e2e_list_sessions() {
    if !stdio_e2e_enabled() {
        eprintln!("Skipping stdio e2e; set ASTRCODE_RUN_STDIO_E2E=1 to run it");
        return;
    }

    let bin = server_binary();
    if !std::path::Path::new(&bin).exists() {
        eprintln!("Skipping: server binary not found");
        return;
    }

    let transport = StdioClientTransport::spawn(&bin, &[]).unwrap();
    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    // 发送 ListSessions 命令
    client
        .send_command(&ClientCommand::ListSessions)
        .await
        .unwrap();

    // 验证收到 SessionList 通知
    match stream.recv().await.unwrap() {
        astrcode_client::stream::StreamItem::Event(ClientNotification::SessionList {
            sessions,
        }) => {
            println!("Sessions: {:?}", sessions);
        },
        other => panic!("Expected SessionList, got: {:?}", other),
    }
}
