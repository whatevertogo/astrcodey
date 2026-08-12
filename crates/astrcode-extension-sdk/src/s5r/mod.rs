//! 扩展线缆协议共享类型 — s5r Peer 线协议（stdio 长度前缀帧 + JSON）。

pub use astrcode_extension_contract::{
    CallContinuation, HandlerEffect, HandlerResult,
    protocol::{
        ActivateMsg, ActivateOutput, CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, CancelMsg, ErrorPayload,
        HandlerId, HandlerInvokeRequest, HandlerKind, InitializeMsg, InitializeOutput, InvokeMsg,
        PeerInfo, ResultKind, ResultMsg, S5R_STACK, S5R_VERSION, StreamMsg, WIRE_CODEC_JSON,
        WireMessage, encode_wire_message, parse_wire_message,
    },
};
