//! Stable wire contract shared by AstrCode extension hosts and workers.
//!
//! This crate intentionally has no dependency on host domain crates. It owns values only when
//! they cross the S5R boundary and the transport state needed to exchange those values.

pub mod capability;
pub mod custom_event;
pub mod effects;
pub mod error;
pub mod extension_http;
pub mod frame;
pub mod host;
pub mod manifest;
pub mod operation;
pub mod peer;
pub mod peer_runtime;
pub mod protocol;
pub mod session;
pub mod session_inspect;
pub mod stream;

pub use capability::ExtensionCapability;
pub use custom_event::{CustomEventDeclaration, CustomEventSourceFilter, CustomEventSubscription};
pub use effects::{CallContinuation, HandlerEffect, HandlerResult};
pub use error::WireErrorCode;
pub use frame::{FrameTransport, ProcessStdioTransport, StdioFrameTransport};
pub use manifest::{CompactEvent, HookMode, LifecycleEvent};
pub use operation::{
    HOST_OPERATION_SPECS, HostBackendRequirement, HostContextRequirement, HostOp, HostOperation,
    HostOperationGroup, HostOperationSpec, operations,
};
pub use peer::{Peer, PeerError, PeerHandshake, Ready, Uninitialized};
pub use peer_runtime::{
    InboundInvoke, InvocationResponse, InvokeError, ModelEventStream, NestedPeer, PeerDriver,
    PeerHandle, PeerInvokeHandler, PeerStream, model_event_stream,
};
pub use protocol::{
    ErrorPayload, FeatureName, HandlerId, HandlerInvokeRequest, HandlerKind, InitializeMsg,
    InitializeOutput, PeerInfo, WireMessage,
};
pub use stream::TerminalStream;
