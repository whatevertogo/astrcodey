//! WASM ABI 协议绑定 — 重导出 s5r 协议类型，供宿主和 guest 双侧共享。

pub use crate::s5r::{
    effects::{CallContinuation, HandlerResult},
    event_from_name, event_to_name, mode_from_name, CapabilityDescriptor, HandlerDescriptor,
    PeerInfo, S5R_STACK, S5R_VERSION, WireMessage, CAP_HANDLER_INVOKE,
};
