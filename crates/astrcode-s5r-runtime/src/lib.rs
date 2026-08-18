//! S5R subprocess extension runtime.
//!
//! Owns the peer state machine ([`peer`]) and the explicitly-owned I/O driver
//! ([`peer_runtime`]) that host and worker both consume. The wire *contract*
//! (messages, framing, operation catalog) stays in
//! `astrcode_extension_sdk::wire`; this crate turns a ready [`Peer`] into a
//! running subprocess session.
//!
//! It lives outside the SDK because the SDK is the authoring surface, while
//! this is runtime machinery shared by `astrcode-extensions` (host) and
//! `astrcode-extension-worker` (worker); moving it into either side would
//! invert the host/worker dependency direction.

mod frame;
pub mod peer;
pub mod peer_runtime;

pub use peer::{
    HostInitialization, HostInitialized, Peer, PeerError, Ready, Uninitialized,
    WorkerInitialization, WorkerInitialized, WorkerReady,
};
pub use peer_runtime::{
    InboundInvoke, InvocationCancellation, InvocationResponse, InvokeError, ModelEventStream,
    PeerDriver, PeerHandle, PeerInvokeHandler, PeerStream,
};
