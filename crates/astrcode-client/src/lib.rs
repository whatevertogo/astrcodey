//! astrcode-client：RPC 客户端库。
//!
//! 提供类型化的 JSON-RPC 客户端，支持传输层抽象、事件流订阅和认证状态管理。
//! 核心组件包括：
//! - [`client`] — 类型化 RPC 客户端，封装会话管理与命令发送。
//! - [`transport`] — 传输层抽象（stdio 等）及错误类型。
//! - [`stream`] — 服务端事件流的异步接收器。
//! - [`error`] — 客户端错误类型定义。

pub mod client;
pub mod error;
pub mod stream;
pub mod transport;
