//! S5R wire protocol types shared between the host and out-of-process extensions.
//!
//! S5R is the name of AstrCode's versioned subprocess extension protocol: a disk
//! extension runs as a separate process and talks to the host over stdio using
//! decimal-length-prefixed JSON frames (see [`crate::wire::protocol`] and
//! [`crate::wire::frame`]). Version negotiation, feature negotiation, handler
//! invocation, streaming, and cancellation ship together as protocol 3.0.
//!
//! This module carries the author-facing S5R DTOs (`hooks`, `tool_plan`). The
//! canonical protocol types themselves live in [`crate::wire`] and
//! [`crate::wire::protocol`]; this module does not re-export them, so every
//! wire type has exactly one home.

mod tool_plan;

pub mod hooks;

pub use tool_plan::{
    FileOperationDto, HostResourceDto, ResourceAccessDto, ToolInvocationPhase,
    ToolInvocationRequest, ToolInvocationScope, ToolPlanDto,
};
