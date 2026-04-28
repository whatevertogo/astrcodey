//! UI 子协议类型模块——服务器发起的用户交互请求。
//!
//! 从 [`commands`](crate::commands) 和 [`events`](crate::events) 模块
//! 重新导出 UI 相关类型，方便外部 crate 统一通过 `astrcode_protocol::ui` 路径引用。

/// 客户端对 UI 请求的响应值类型。
pub use crate::commands::UiResponseValue;
/// 服务器推送的客户端通知（包含 UI 请求）和 UI 请求类型。
pub use crate::events::{ClientNotification, UiRequestKind};
