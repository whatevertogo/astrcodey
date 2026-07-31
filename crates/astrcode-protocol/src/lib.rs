//! astrcode-protocol：线缆协议类型 crate。
//!
//! 定义两套外部契约的线缆（wire）类型：
//!
//! - **stdio JSON-RPC**（`commands` / `events` / `framing` / `transport` / `version`）： TUI
//!   等进程内客户端与服务端之间的 JSON-RPC 消息、JSONL 帧格式与版本协商。
//! - **HTTP/SSE**（`http` / `wire`）：Web 前端消费的 REST 请求/响应 DTO 与 SSE 增量， TypeScript
//!   绑定由 `examples/generate-typescript.rs` 生成。
//!
//! 本 crate 仅包含协议数据类型定义，不包含任何业务逻辑。

pub mod agent_session_link;
pub mod commands;
pub mod events;
pub mod framing;
pub mod http;
pub mod transport;
pub mod version;
pub mod wire;
