//! 无头执行模式 —— 单次提示执行（进程内）。
//!
//! 该模块实现了 CLI 的 `exec` 子命令，用于在不需要交互式 TUI 的情况下
//! 一次性提交提示并输出结果。支持纯文本和 JSONL 两种输出格式。

use astrcode_client::client::AstrcodeClient;
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

use crate::transport::InProcessTransport;

/// 执行单次提示并等待响应完成。
///
/// # 参数
///
/// - `prompt`: 用户输入的提示文本
/// - `jsonl`: 是否以 JSONL 格式输出事件（每行一个 JSON 对象）
/// - `timeout_secs`: 超时时间（秒），0 表示不超时
///
/// # 返回值
///
/// 成功返回 `Ok(())`，失败返回错误描述字符串。
pub async fn run(prompt: &str, jsonl: bool, timeout_secs: u64) -> Result<(), String> {
    // 使用进程内传输启动服务器，避免子进程开销
    let client = AstrcodeClient::new(InProcessTransport::start());

    // 在当前工作目录创建新会话
    let _sid = client
        .create_session(".")
        .await
        .map_err(|e| format!("Cannot create session: {e}"))?;

    // 订阅服务器事件流，用于接收响应
    let mut stream = client
        .subscribe_events()
        .await
        .map_err(|e| format!("Cannot subscribe: {e}"))?;

    // 提交用户提示
    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: prompt.into(),
            attachments: vec![],
        })
        .await
        .map_err(|e| format!("Cannot submit: {e}"))?;

    // 仅在 timeout_secs > 0 时设置截止时间
    let deadline = (timeout_secs > 0)
        .then(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs));

    loop {
        // 根据是否设置了超时来选择接收方式
        let recv_result = if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, stream.recv())
                .await
                .map_err(|_| format!("exec timed out after {timeout_secs}s"))?
        } else {
            stream.recv().await
        };
        match recv_result {
            Ok(astrcode_client::stream::StreamItem::Event(event)) => match event {
                ClientNotification::Event(core_event) => match core_event.payload {
                    // 助手文本增量：非 JSONL 模式直接打印到标准输出
                    EventPayload::AssistantTextDelta { delta, .. } => {
                        if !jsonl {
                            print!("{delta}");
                        }
                    },
                    // 对话轮次完成：结束执行循环
                    EventPayload::TurnCompleted { .. } => {
                        if !jsonl {
                            println!();
                        }
                        break;
                    },
                    // 错误事件：打印到标准错误并退出
                    EventPayload::ErrorOccurred { message, .. } => {
                        eprintln!("Error: {message}");
                        break;
                    },
                    // 其他事件：仅在 JSONL 模式下输出
                    _ => {
                        if jsonl {
                            println!(
                                "{}",
                                serde_json::to_string(&ClientNotification::Event(core_event))
                                    .unwrap_or_default()
                            );
                        }
                    },
                },
                ClientNotification::Error { message, .. } => {
                    eprintln!("Error: {message}");
                    break;
                },
                other => {
                    if jsonl {
                        println!("{}", serde_json::to_string(&other).unwrap_or_default());
                    }
                },
            },
            // 事件流滞后（消费速度跟不上生产速度），静默忽略
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {},
            Err(_) => break,
        }
    }
    Ok(())
}
