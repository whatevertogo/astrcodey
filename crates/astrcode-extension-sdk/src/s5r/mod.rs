//! s5r 对称 peer 协议 — WASM 扩展与宿主之间的线缆契约。
//!
//! 协议版本 [`S5R_VERSION`] = `"1.0"`。传输层使用单一 `peer_exchange` 交换
//! [`WireMessage`] JSON；握手后 guest 通过 `astrcode.*` 能力 invoke 宿主，
//! 宿主通过 `handler.invoke` 调用 guest 注册的 handler。

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
// Re-export event/mode helpers (same mapping as former s6r).
pub use messages::{event_from_name, event_to_name, mode_from_name};
