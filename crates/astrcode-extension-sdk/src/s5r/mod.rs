//! S5R wire protocol types shared between the host and out-of-process extensions.
//!
//! S5R is the name of AstrCode's versioned subprocess extension protocol: a disk
//! extension runs as a separate process and talks to the host over stdio using
//! decimal-length-prefixed JSON frames (see [`crate::wire::protocol`] and
//! [`crate::wire::frame`]). Version negotiation, feature negotiation, handler
//! invocation, streaming, and cancellation ship together as protocol 3.0.
//!
//! This module is the extension-facing half of that protocol. The host-side
//! implementation lives in `astrcode-extensions::s5r_ext`, and extension authors
//! normally consume these types through `astrcode-extension-worker` instead of
//! importing the protocol directly.

mod tool_plan;

pub mod hooks;

pub use tool_plan::{
    FileOperationDto, HostResourceDto, ResourceAccessDto, ToolInvocationPhase,
    ToolInvocationRequest, ToolInvocationScope, ToolPlanDto,
};

pub use crate::wire::{
    CallContinuation, HandlerEffect, HandlerResult, ProviderContributionData,
    ProviderContributionEffect,
    protocol::{
        ActivateMsg, ActivateOutput, CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, CancelMsg, ErrorPayload,
        HandlerId, HandlerInvokeRequest, HandlerKind, InitializeMsg, InitializeOutput, InvokeMsg,
        PeerInfo, ResultKind, ResultMsg, S5R_STACK, S5R_VERSION, StreamMsg, WIRE_CODEC_JSON,
        WireMessage, encode_wire_message, parse_wire_message,
    },
};
