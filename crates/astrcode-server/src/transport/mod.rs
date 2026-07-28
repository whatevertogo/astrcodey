//! 传输层：stdio JSON-RPC 实现。

mod stdio;

pub use astrcode_protocol::transport::TransportError;
pub use stdio::{StdioTransport, write_error_response, write_initialize_response};
