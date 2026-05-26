//! 扩展线缆协议共享类型 — IPC 子进程与宿主之间的 JSON 载荷契约。
//!
//! 命名保留历史 `s5r` 前缀；传输层现为 stdio JSON-RPC / JSONL（`protocol.ipc`）。

pub mod capabilities;
pub mod effects;
pub mod messages;

pub use capabilities::{
    astrcode_capability_name, capability_from_wire, capability_to_wire, is_astrcode_capability,
    is_reserved_capability_prefix,
};
pub use effects::{CallContinuation, HandlerResult};
pub use messages::{
    CAP_HANDLER_INVOKE, CapabilityDescriptor, ErrorPayload, EventMsg, EventPhase,
    HandlerDescriptor, InitializeMsg, InitializeOutput, InvokeMsg, PeerInfo, ResultKind, ResultMsg,
    S5R_STACK, S5R_VERSION, WireMessage,
};
pub use messages::{event_from_name, event_to_name, mode_from_name};
