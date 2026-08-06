//! 扩展线缆协议共享类型 — s5r Peer 线协议（stdio 长度前缀帧 + JSON）。

pub mod capabilities;
pub mod effects;
pub mod manifest;
pub mod messages;

pub use capabilities::{
    astrcode_capability_name, capability_from_wire, capability_to_wire, is_astrcode_capability,
    is_reserved_capability_prefix,
};
pub use effects::{CallContinuation, HandlerResult};
pub use messages::{
    CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, CancelMsg, CapabilityDescriptor, ErrorPayload, EventMsg,
    EventPhase, HandlerDescriptor, InitializeMsg, InitializeOutput, InvokeMsg, PeerInfo,
    ResultKind, ResultMsg, S5R_STACK, S5R_VERSION, WIRE_CODEC_JSON, WIRE_CODEC_METADATA_KEY,
    WIRE_FEATURE_PARENT_INVOKE_ID, WireMessage, encode_wire_message, event_from_name,
    event_to_name, mode_from_name, mode_to_name, parse_wire_message,
};
