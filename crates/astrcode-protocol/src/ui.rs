//! UI sub-protocol types (server-initiated user interaction requests).

pub use crate::events::{ServerEvent, UiRequestKind};

/// Re-exported for convenience — UI types are part of the event stream.
pub use crate::commands::UiResponseValue;
