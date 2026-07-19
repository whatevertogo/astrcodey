//! astrcode-protocol：线缆协议类型 crate。
//!
//! 定义 JSON-RPC 2.0 消息类型，包括客户端命令、服务器事件通知、
//! 会话快照以及协议版本协商等线缆（wire）传输类型。
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
