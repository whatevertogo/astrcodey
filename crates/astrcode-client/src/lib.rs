//! astrcode-client：进程内 RPC 客户端库。
//!
//! 供 TUI / exec 通过 [`ClientTransport`]（如 `InProcessTransport`）与同一进程内的
//! server runtime 通信。外部集成请使用 HTTP/SSE API。

pub mod client;
pub mod error;
pub mod stream;
pub mod transport;
