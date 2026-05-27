//! astrcode-protocol：线缆协议类型 crate。
//!
//! - **进程内**：[`commands::ClientCommand`] / [`events::ClientNotification`]
//! - **HTTP/SSE**：[`http`] 模块中的 REST 与 SSE delta DTO
//!
//! 本 crate 仅包含协议数据类型定义，不包含任何业务逻辑。

pub mod agent_session_link;
pub mod commands;
pub mod events;
pub mod framing;
pub mod http;
pub mod transport;
