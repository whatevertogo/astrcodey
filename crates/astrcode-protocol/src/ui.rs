//! UI sub-protocol types (server-initiated user interaction requests).

/// Re-exported for convenience — UI types are part of the event stream.
pub use crate::commands::UiResponseValue;
pub use crate::events::{ClientNotification, UiRequestKind};
